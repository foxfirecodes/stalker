//! CLI parsing and validated runtime configuration.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use clap::{ArgAction, Parser};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};

use crate::{
    CommandId, CommandSpec, RunEvent, RunEventKind, RunStatus,
    filter::PathFilter,
    output::{OutputMode as RenderOutputMode, OutputRenderer},
    remote::{RemoteSpec, RemoteWatcher},
    runner::ChildRunner,
    scheduler::Scheduler,
    watcher::{NativeWatcher, WatchEvent},
};

/// Command line arguments before path and glob validation.
#[derive(Debug, Parser)]
#[command(
    name = "stalker",
    version,
    about = "Rerun a command after filesystem changes"
)]
struct Cli {
    /// SSH destination whose filesystem should be watched. The command stays local.
    #[arg(long, value_name = "DESTINATION")]
    remote: Option<String>,

    /// Remote base directory for watch paths and globs. Defaults to the SSH login directory.
    #[arg(long, value_name = "PATH", requires = "remote")]
    remote_cwd: Option<PathBuf>,

    /// OpenSSH configuration option (KEY=VALUE); repeat to supply more than one.
    #[arg(long = "ssh-option", value_name = "KEY=VALUE", requires = "remote")]
    ssh_options: Vec<String>,

    /// Child working directory. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    cwd: Option<PathBuf>,

    /// A file or directory to watch. May be supplied more than once.
    #[arg(long = "watch", value_name = "PATH", required = true)]
    watch: Vec<PathBuf>,

    /// A relevant path glob, relative to --cwd.
    #[arg(long = "include", value_name = "GLOB")]
    include: Vec<String>,

    /// An ignored path glob, relative to --cwd.
    #[arg(long = "ignore", value_name = "GLOB")]
    ignore: Vec<String>,

    /// The quiet period before a rerun.
    #[arg(long, default_value = "150ms", value_name = "DURATION", value_parser = parse_duration)]
    debounce: Duration,

    /// Run once on startup. This is the default.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_initial_run")]
    initial_run: bool,

    /// Wait for the first relevant filesystem event before running.
    #[arg(long = "no-initial-run", action = ArgAction::SetTrue, conflicts_with = "initial_run")]
    no_initial_run: bool,

    /// Stop watching after the command exits successfully (exit code 0).
    #[arg(long, action = ArgAction::SetTrue)]
    until_success: bool,

    /// Frame each run with Stalker marker lines.
    #[arg(long, action = ArgAction::SetTrue)]
    markers: bool,

    /// Attach child output directly to the terminal, preserving terminal-aware output such as color.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "markers")]
    raw_output: bool,

    /// Write accepted filesystem events to stderr.
    #[arg(long = "print-events", action = ArgAction::SetTrue)]
    print_events: bool,

    /// Do not apply repository .gitignore rules.
    #[arg(long = "no-gitignore", action = ArgAction::SetTrue)]
    no_gitignore: bool,

    /// The command to run, separated from Stalker options by `--`.
    #[arg(
        last = true,
        required = true,
        value_name = "COMMAND",
        allow_hyphen_values = true
    )]
    command: Vec<OsString>,
}

/// Validated configuration used by the runner.
#[derive(Debug)]
pub struct Config {
    pub remote: Option<RemoteSpec>,
    pub commands: Vec<CommandSpec>,
    pub watch: WatchSpec,
    pub output: OutputMode,
    pub raw_output: bool,
    pub initial_run: bool,
    pub until_success: bool,
    pub print_events: bool,
    pub gitignore: bool,
}

pub use stalker_watch::config::WatchSpec;
use stalker_watch::config::{
    canonical_directory, canonical_watch_path, compile_globs, compile_ignore_globs,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Passthrough,
    Markers,
}

impl Config {
    /// Parse process arguments and validate paths and globs.
    pub fn parse() -> Result<Self> {
        Self::from_cli(Cli::parse())
    }

    /// Parse the supplied arguments. Useful for embedding and tests.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::from_cli(Cli::try_parse_from(args).map_err(anyhow::Error::from)?)
    }

    fn from_cli(cli: Cli) -> Result<Self> {
        let cwd = canonical_directory(cli.cwd.as_deref().unwrap_or_else(|| Path::new(".")))?;
        if cli
            .remote
            .as_ref()
            .is_some_and(|destination| destination.is_empty() || destination.starts_with('-'))
        {
            bail!("remote must be an SSH destination, such as user@host or ssh://user@host:port");
        }
        let roots = if cli.remote.is_some() {
            // Only the remote helper can resolve these paths and expand its HOME.
            cli.watch
        } else {
            cli.watch
                .iter()
                .map(|path| canonical_watch_path(&cwd, path))
                .collect::<Result<Vec<_>>>()?
        };
        let includes = compile_globs(&cli.include, "include")?;
        let ignores = compile_ignore_globs(&cli.ignore)?;

        let mut command = cli.command.into_iter();
        let program = PathBuf::from(command.next().expect("clap requires a command"));
        let command_spec = CommandSpec {
            id: CommandId("default".to_owned()),
            program,
            args: command.collect(),
            cwd,
        };

        Ok(Self {
            remote: cli.remote.map(|destination| RemoteSpec {
                destination,
                cwd: cli.remote_cwd,
                ssh_options: cli.ssh_options,
            }),
            commands: vec![command_spec],
            watch: WatchSpec {
                roots,
                includes,
                ignores,
                include_patterns: cli.include,
                ignore_patterns: cli.ignore,
                debounce: cli.debounce,
            },
            output: if cli.markers {
                OutputMode::Markers
            } else {
                OutputMode::Passthrough
            },
            raw_output: cli.raw_output,
            initial_run: !cli.no_initial_run,
            until_success: cli.until_success,
            print_events: cli.print_events,
            gitignore: !cli.no_gitignore,
        })
    }
}

/// Parse the duration syntax accepted by the MVP CLI.
pub fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let (number, unit) = if let Some(number) = value.strip_suffix("ms") {
        (number, "ms")
    } else if let Some(number) = value.strip_suffix('s') {
        (number, "s")
    } else if let Some(number) = value.strip_suffix('m') {
        (number, "m")
    } else if let Some(number) = value.strip_suffix('h') {
        (number, "h")
    } else {
        return Err("duration must end in ms, s, m, or h (for example, 150ms)".to_owned());
    };

    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("duration must be a non-negative whole number".to_owned());
    }
    let number = number
        .parse::<u64>()
        .map_err(|_| "duration is too large".to_owned())?;
    let seconds = match unit {
        "ms" => return Ok(Duration::from_millis(number)),
        "s" => number,
        "m" => number
            .checked_mul(60)
            .ok_or_else(|| "duration is too large".to_owned())?,
        "h" => number
            .checked_mul(60 * 60)
            .ok_or_else(|| "duration is too large".to_owned())?,
        _ => unreachable!(),
    };
    Ok(Duration::from_secs(seconds))
}

pub fn run() -> Result<()> {
    let config = Config::parse()?;
    let command = config.commands[0].clone();
    let stopping = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stopping))?;
    flag::register(SIGTERM, Arc::clone(&stopping))?;
    // Start watching before scheduling the initial run so a change in that
    // startup window cannot be missed.
    let watcher = if let Some(remote) = &config.remote {
        use stalker_watch::protocol::{self, WatchRequest, WirePath};
        EventSource::Remote(RemoteWatcher::new(
            remote,
            WatchRequest {
                version: protocol::VERSION,
                cwd: remote.cwd.as_deref().map(WirePath::new),
                roots: config
                    .watch
                    .roots
                    .iter()
                    .map(|path| WirePath::new(path))
                    .collect(),
                includes: config.watch.include_patterns.clone(),
                ignores: config.watch.ignore_patterns.clone(),
                gitignore: config.gitignore,
            },
            &stopping,
        )?)
    } else {
        EventSource::Native {
            filter: PathFilter::from_watch_spec(&command.cwd, &config.watch, config.gitignore)?,
            watcher: NativeWatcher::new(config.watch.roots.clone())?,
        }
    };
    let (event_sender, event_receiver) = crossbeam_channel::unbounded();
    let runner = ChildRunner::new(event_sender, config.raw_output);
    let mut scheduler = Scheduler::new(config.watch.debounce, config.initial_run);
    let mut output = OutputRenderer::terminal(match config.output {
        OutputMode::Passthrough => RenderOutputMode::Passthrough,
        OutputMode::Markers => RenderOutputMode::Markers,
    });
    let outcome = (|| -> Result<()> {
        while !stopping.load(Ordering::Relaxed) {
            drain_watch_events(&watcher, &mut scheduler, config.print_events)?;
            start_due_run(&mut scheduler, &runner, &command);
            let succeeded = drain_run_events(&event_receiver, &mut output, &mut scheduler)?;
            if config.until_success && succeeded {
                break;
            }

            let wait = scheduler
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or(Duration::from_millis(25))
                .min(Duration::from_millis(25));
            match event_receiver.recv_timeout(wait) {
                Ok(event) => {
                    let succeeded = handle_run_event(event, &mut output, &mut scheduler)?;
                    if config.until_success && succeeded {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!("run event channel disconnected"),
            }
        }
        Ok(())
    })();

    // Transport failures must also stop an active local command and its children.
    runner.shutdown();
    let graceful_deadline = std::time::Instant::now() + Duration::from_millis(500);
    while runner.is_active() && std::time::Instant::now() < graceful_deadline {
        if let Ok(event) = event_receiver.recv_timeout(Duration::from_millis(25)) {
            handle_run_event(event, &mut output, &mut scheduler)?;
        }
    }
    if runner.is_active() {
        runner.force_shutdown();
    }
    while runner.is_active() {
        if let Ok(event) = event_receiver.recv_timeout(Duration::from_millis(25)) {
            handle_run_event(event, &mut output, &mut scheduler)?;
        }
    }
    drain_run_events(&event_receiver, &mut output, &mut scheduler)?;
    outcome
}

enum EventSource {
    Native {
        watcher: NativeWatcher,
        filter: PathFilter,
    },
    Remote(RemoteWatcher),
}

impl EventSource {
    fn try_recv(&self) -> Result<Option<WatchEvent>> {
        match self {
            Self::Remote(watcher) => watcher.try_recv(),
            Self::Native { watcher, filter } => {
                for _ in 0..256 {
                    let Some(event) = watcher.try_recv()? else {
                        break;
                    };
                    if event_is_relevant(&event, filter) {
                        return Ok(Some(event));
                    }
                }
                Ok(None)
            }
        }
    }
}

use stalker_watch::event_is_relevant;

fn start_due_run(scheduler: &mut Scheduler, runner: &ChildRunner, command: &CommandSpec) {
    if let Some(run) = scheduler.take_due_run(std::time::Instant::now()) {
        runner.start(command.clone(), run.id, run.trigger);
    }
}

fn drain_watch_events(
    watcher: &EventSource,
    scheduler: &mut Scheduler,
    print_events: bool,
) -> Result<()> {
    // Give command completions and signals a turn even during continuous writes.
    for _ in 0..256 {
        let Some(event) = watcher.try_recv()? else {
            break;
        };
        if print_events {
            print_event(&event)?;
        }
        scheduler.change(std::time::Instant::now());
    }
    Ok(())
}

fn print_event(event: &WatchEvent) -> io::Result<()> {
    use std::io::Write;

    let mut stderr = io::stderr().lock();
    if event.is_overflow {
        writeln!(stderr, "stalker: accepted filesystem overflow/rescan")
    } else if event.paths.is_empty() {
        writeln!(stderr, "stalker: accepted broad filesystem event")
    } else {
        for path in &event.paths {
            writeln!(
                stderr,
                "stalker: accepted filesystem event {}",
                path.display()
            )?;
        }
        Ok(())
    }
}

fn drain_run_events(
    events: &Receiver<RunEvent>,
    output: &mut OutputRenderer<io::Stdout, io::Stderr>,
    scheduler: &mut Scheduler,
) -> Result<bool> {
    let mut succeeded = false;
    while let Ok(event) = events.try_recv() {
        succeeded |= handle_run_event(event, output, scheduler)?;
    }
    Ok(succeeded)
}

/// Render an event and report whether the active run completed successfully.
fn handle_run_event(
    event: RunEvent,
    output: &mut OutputRenderer<io::Stdout, io::Stderr>,
    scheduler: &mut Scheduler,
) -> Result<bool> {
    let completed_run = match event.kind {
        RunEventKind::Finished { .. } | RunEventKind::SpawnFailed { .. } => Some(event.run_id),
        RunEventKind::Started { .. } | RunEventKind::Output { .. } => None,
    };
    output.render(&event)?;
    if let Some(run_id) = completed_run {
        let completed = scheduler.finish(run_id, std::time::Instant::now());
        return Ok(completed
            && matches!(
                event.kind,
                RunEventKind::Finished {
                    status: RunStatus::ExitCode(0)
                }
            ));
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::PathKind;
    use globset::Glob;
    use tempfile::tempdir;

    #[test]
    fn remote_paths_are_resolved_remotely_while_cwd_stays_local() {
        let temp = tempdir().unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--remote",
            "ssh://user@host:2222",
            "--remote-cwd",
            "~/remote-project",
            "--watch",
            "does-not-exist-locally",
            "--",
            "true",
        ])
        .unwrap();
        assert_eq!(
            config.watch.roots,
            vec![PathBuf::from("does-not-exist-locally")]
        );
        assert_eq!(config.commands[0].cwd, temp.path().canonicalize().unwrap());
        assert_eq!(
            config.remote.unwrap().cwd.unwrap(),
            PathBuf::from("~/remote-project")
        );
        assert!(
            Config::try_parse_from(["stalker", "--remote-cwd", ".", "--watch", ".", "--", "true"])
                .is_err()
        );
        assert!(
            Config::try_parse_from([
                "stalker",
                "--remote=-oProxyCommand=bad",
                "--watch",
                ".",
                "--",
                "true"
            ])
            .is_err()
        );
    }

    #[test]
    fn duration_parser_accepts_mvp_units() {
        assert_eq!(parse_duration("150ms").unwrap(), Duration::from_millis(150));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("150").is_err());
        assert!(parse_duration("1.5s").is_err());
    }

    #[test]
    fn config_defaults_to_an_initial_run() {
        let temp = tempdir().unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--watch",
            ".",
            "--",
            "echo",
            "hello",
        ])
        .unwrap();

        assert!(config.initial_run);
        assert!(!config.until_success);
        assert_eq!(config.commands[0].program, PathBuf::from("echo"));
        assert_eq!(config.commands[0].args, [OsString::from("hello")]);
    }

    #[test]
    fn no_initial_run_disables_startup_run() {
        let temp = tempdir().unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--watch",
            ".",
            "--no-initial-run",
            "--",
            "echo",
        ])
        .unwrap();

        assert!(!config.initial_run);
    }

    #[test]
    fn until_success_can_wait_for_the_first_change() {
        let temp = tempdir().unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--watch",
            ".",
            "--until-success",
            "--no-initial-run",
            "--",
            "echo",
        ])
        .unwrap();

        assert!(config.until_success);
        assert!(!config.initial_run);
    }

    #[test]
    fn raw_output_attaches_child_streams_to_the_terminal() {
        let temp = tempdir().unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--watch",
            ".",
            "--raw-output",
            "--",
            "echo",
        ])
        .unwrap();

        assert!(config.raw_output);
    }

    #[test]
    fn plain_ignore_matches_descendant_events() {
        let temp = tempdir().unwrap();
        std::fs::create_dir(temp.path().join("target")).unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--watch",
            ".",
            "--ignore",
            "target",
            "--",
            "echo",
        ])
        .unwrap();
        let filter =
            PathFilter::from_watch_spec(&config.commands[0].cwd, &config.watch, config.gitignore)
                .unwrap();

        assert!(!filter.accepts(&temp.path().join("target/output.o"), PathKind::File));
    }

    #[test]
    fn no_gitignore_opt_out_keeps_gitignored_paths_relevant() {
        let temp = tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::create_dir(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join(".gitignore"), "target/\n").unwrap();
        let config = Config::try_parse_from([
            "stalker",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--watch",
            ".",
            "--no-gitignore",
            "--",
            "echo",
        ])
        .unwrap();
        let filter =
            PathFilter::from_watch_spec(&config.commands[0].cwd, &config.watch, config.gitignore)
                .unwrap();
        let event_path = config.commands[0].cwd.join("target/output.o");

        assert!(!config.gitignore);
        assert_eq!(
            filter.normalize(&event_path),
            Some(PathBuf::from("target/output.o"))
        );
        assert!(filter.accepts_normalized(Path::new("target/output.o"), PathKind::File));
        assert!(filter.accepts(&event_path, PathKind::File));
    }

    #[test]
    fn empty_broad_event_schedules_conservatively() {
        let filter = PathFilter::new(
            PathBuf::from("/repo"),
            &[Glob::new("src/**/*.rs").unwrap()],
            &[],
        )
        .unwrap();
        let event = WatchEvent {
            paths: Vec::new(),
            is_directory: true,
            is_broad: true,
            is_overflow: false,
        };

        assert!(event_is_relevant(&event, &filter));
    }
}

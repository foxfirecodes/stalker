//! CLI parsing and validated runtime configuration.

use std::{
    env,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use globset::Glob;
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};

use crate::{
    CommandId, CommandSpec, RunEvent, RunEventKind,
    filter::{PathFilter, PathKind},
    output::{OutputMode as RenderOutputMode, OutputRenderer},
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

    /// Frame each run with Stalker marker lines.
    #[arg(long, action = ArgAction::SetTrue)]
    markers: bool,

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
    pub commands: Vec<CommandSpec>,
    pub watch: WatchSpec,
    pub output: OutputMode,
    pub initial_run: bool,
    pub print_events: bool,
    pub gitignore: bool,
}

/// Filesystem roots and matching rules shared by all commands in the MVP.
#[derive(Debug)]
pub struct WatchSpec {
    /// Existing, canonical roots passed to the native watcher.
    pub roots: Vec<PathBuf>,
    /// Compiled include patterns, relative to `cwd`.
    pub includes: Vec<Glob>,
    /// Compiled ignore patterns, relative to `cwd`.
    pub ignores: Vec<Glob>,
    /// Original include text, retained for conservative directory matching.
    pub include_patterns: Vec<String>,
    /// Original ignore text, retained for conservative directory matching.
    pub ignore_patterns: Vec<String>,
    pub debounce: Duration,
}

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
        let roots = cli
            .watch
            .iter()
            .map(|path| canonical_watch_path(&cwd, path))
            .collect::<Result<Vec<_>>>()?;
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
            initial_run: !cli.no_initial_run,
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

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()?.join(path)
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("could not resolve working directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("working directory {} is not a directory", path.display());
    }
    Ok(path)
}

fn canonical_watch_path(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .with_context(|| format!("could not resolve watch path {}", path.display()))
}

fn compile_globs(patterns: &[String], kind: &str) -> Result<Vec<Glob>> {
    patterns
        .iter()
        .map(|pattern| {
            Glob::new(pattern).with_context(|| format!("invalid {kind} glob {pattern:?}"))
        })
        .collect()
}

/// A plain directory name is the common spelling for ignoring that directory.
/// Keep its exact match and add descendants so `--ignore target` behaves like
/// `--ignore target/**` for file events.
fn compile_ignore_globs(patterns: &[String]) -> Result<Vec<Glob>> {
    let mut expanded = Vec::with_capacity(patterns.len() * 2);
    for pattern in patterns {
        expanded.push(pattern.clone());
        let directory = pattern.trim_end_matches('/');
        if !directory.is_empty()
            && !directory.contains(['*', '?', '[', '{'])
            && !pattern.ends_with("/**")
        {
            expanded.push(format!("{directory}/**"));
        }
    }
    compile_globs(&expanded, "ignore")
}

pub fn run() -> Result<()> {
    let config = Config::parse()?;
    let command = config.commands[0].clone();
    let filter = PathFilter::from_watch_spec(&command.cwd, &config.watch, config.gitignore)?;
    // Start watching before scheduling the initial run so a change in that
    // startup window cannot be missed.
    let watcher = NativeWatcher::new(config.watch.roots.clone())?;
    let (event_sender, event_receiver) = crossbeam_channel::unbounded();
    let runner = ChildRunner::new(event_sender);
    let mut scheduler = Scheduler::new(config.watch.debounce, config.initial_run);
    let mut output = OutputRenderer::terminal(match config.output {
        OutputMode::Passthrough => RenderOutputMode::Passthrough,
        OutputMode::Markers => RenderOutputMode::Markers,
    });
    let stopping = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stopping))?;
    flag::register(SIGTERM, Arc::clone(&stopping))?;

    while !stopping.load(Ordering::Relaxed) {
        start_due_run(&mut scheduler, &runner, &command);
        drain_watch_events(&watcher, &filter, &mut scheduler, config.print_events)?;
        drain_run_events(&event_receiver, &mut output, &mut scheduler)?;

        let wait = scheduler
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(Duration::from_millis(25))
            .min(Duration::from_millis(25));
        match event_receiver.recv_timeout(wait) {
            Ok(event) => handle_run_event(event, &mut output, &mut scheduler)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("run event channel disconnected"),
        }
    }

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
    Ok(())
}

fn start_due_run(scheduler: &mut Scheduler, runner: &ChildRunner, command: &CommandSpec) {
    if let Some(run) = scheduler.take_due_run(std::time::Instant::now()) {
        runner.start(command.clone(), run.id, run.trigger);
    }
}

fn drain_watch_events(
    watcher: &NativeWatcher,
    filter: &PathFilter,
    scheduler: &mut Scheduler,
    print_events: bool,
) -> Result<()> {
    while let Some(event) = watcher.try_recv()? {
        if !event_is_relevant(&event, filter) {
            continue;
        }
        if print_events {
            print_event(&event)?;
        }
        scheduler.change(std::time::Instant::now());
    }
    Ok(())
}

fn event_is_relevant(event: &WatchEvent, filter: &PathFilter) -> bool {
    if event.is_overflow {
        return true;
    }
    // A backend can report a broad directory-level change without a usable
    // path. It may contain an included descendant, so rerun conservatively.
    if event.is_broad && event.paths.is_empty() {
        return true;
    }
    let kind = if event.is_directory || event.is_broad {
        PathKind::Directory
    } else {
        PathKind::File
    };
    event.paths.iter().any(|path| filter.accepts(path, kind))
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
) -> Result<()> {
    while let Ok(event) = events.try_recv() {
        handle_run_event(event, output, scheduler)?;
    }
    Ok(())
}

fn handle_run_event(
    event: RunEvent,
    output: &mut OutputRenderer<io::Stdout, io::Stderr>,
    scheduler: &mut Scheduler,
) -> Result<()> {
    let completed_run = match event.kind {
        RunEventKind::Finished { .. } | RunEventKind::SpawnFailed { .. } => Some(event.run_id),
        RunEventKind::Started { .. } | RunEventKind::Output { .. } => None,
    };
    output.render(&event)?;
    if let Some(run_id) = completed_run {
        scheduler.finish(run_id, std::time::Instant::now());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::Glob;
    use tempfile::tempdir;

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

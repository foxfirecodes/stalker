//! Thin remote watcher. This binary deliberately does not link the main crate.

use anyhow::{Context, Result, bail};
use crossbeam_channel::{RecvTimeoutError, bounded};
use stalker_watch::{
    config::{
        WatchSpec, canonical_directory, canonical_watch_path, compile_globs, compile_ignore_globs,
    },
    event_is_relevant,
    filter::PathFilter,
    protocol::{self, ControlMessage, ServerMessage, WatchRequest, read_frame, write_frame},
    watcher::NativeWatcher,
};
use std::{
    env, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

fn main() {
    if let Err(error) = run() {
        let message = format!("{error:#}");
        let _ = write_frame(
            &mut io::stdout().lock(),
            &ServerMessage::Error(message.clone()),
        );
        eprintln!("stalker-agent: {message}");
        std::process::exit(1);
    }
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    if path == Path::new("~") {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .context("remote HOME is unset");
    }
    if let Ok(suffix) = path.strip_prefix("~") {
        return Ok(
            PathBuf::from(env::var_os("HOME").context("remote HOME is unset")?).join(suffix),
        );
    }
    Ok(path.to_owned())
}

fn run() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    write_frame(
        &mut output,
        &ServerMessage::Hello {
            version: protocol::VERSION,
        },
    )?;
    let request: WatchRequest =
        read_frame(&mut input)?.context("connection closed before watch configuration")?;
    if request.version != protocol::VERSION {
        bail!("unsupported protocol version {}", request.version);
    }
    if request.roots.is_empty() {
        bail!("no watch roots configured");
    }
    let cwd = match request.cwd {
        Some(path) => canonical_directory(&expand_home(&path.into_path())?)?,
        None => canonical_directory(&env::current_dir()?)?,
    };
    let roots = request
        .roots
        .into_iter()
        .map(|path| canonical_watch_path(&cwd, &expand_home(&path.into_path())?))
        .collect::<Result<Vec<_>>>()?;
    let watch = WatchSpec {
        roots,
        includes: compile_globs(&request.includes, "include")?,
        ignores: compile_ignore_globs(&request.ignores)?,
        include_patterns: request.includes,
        ignore_patterns: request.ignores,
        debounce: Duration::ZERO,
    };
    let filter = PathFilter::from_watch_spec(cwd, &watch, request.gitignore)?;
    let watcher = NativeWatcher::new(watch.roots)?;
    // Drop the configuration reader before another thread takes stdin's lock.
    drop(input);
    let (sender, controls) = bounded(4);
    thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let message = match read_frame::<ControlMessage>(&mut input) {
                Ok(Some(message)) => message,
                _ => ControlMessage::Stop,
            };
            let stop = matches!(message, ControlMessage::Stop);
            if sender.send(message).is_err() || stop {
                break;
            }
        }
    });
    // Keep the lease independent of stdout: a blocked event write must not
    // prevent a dead/disconnected client from being reaped. The shell owns cleanup.
    thread::spawn(move || {
        loop {
            match controls.recv_timeout(Duration::from_secs(45)) {
                Ok(ControlMessage::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    std::process::exit(0)
                }
                Ok(ControlMessage::Ping) => {}
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("stalker-agent: client heartbeat timed out");
                    std::process::exit(1);
                }
            }
        }
    });
    write_frame(&mut output, &ServerMessage::Ready)?;
    loop {
        let event = watcher.recv()?;
        if event_is_relevant(&event, &filter) {
            write_frame(&mut output, &ServerMessage::Change(event.into()))?;
        }
    }
}

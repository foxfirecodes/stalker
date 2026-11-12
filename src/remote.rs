//! System-SSH transport and deployment of embedded remote watchers.

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use sha2::{Digest, Sha256};
use stalker_watch::{
    protocol::{self, ControlMessage, ServerMessage, WatchRequest, read_frame, write_frame},
    watcher::WatchEvent,
};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

include!(concat!(env!("OUT_DIR"), "/helpers.rs"));
const BOOTSTRAP: &str = include_str!("remote-bootstrap.sh");

#[derive(Debug)]
pub struct RemoteSpec {
    pub destination: String,
    pub cwd: Option<PathBuf>,
    pub ssh_options: Vec<String>,
}

enum Message {
    Ready,
    Change(WatchEvent),
    Failed(anyhow::Error),
}

pub struct RemoteWatcher {
    child: Child,
    events: Receiver<Message>,
    stop: Sender<()>,
}

impl RemoteWatcher {
    pub fn new(
        spec: &RemoteSpec,
        request: WatchRequest,
        stopping: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut command = Command::new("ssh");
        // Keep authentication and destination interpretation in OpenSSH. These
        // overrides only ensure that its session remains a foreground byte stream.
        command.args(["-T"]);
        for option in [
            "ClearAllForwardings=yes",
            "RequestTTY=no",
            "RemoteCommand=none",
            "SessionType=default",
            "StdinNull=no",
            "ForkAfterAuthentication=no",
        ] {
            command.args(["-o", option]);
        }
        for option in &spec.ssh_options {
            command.arg("-o").arg(option);
        }
        command.args([
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        command.arg("--").arg(&spec.destination);
        // SSH joins remote argv with spaces. Quote the one fixed script for the
        // login shell, and never interpolate user paths or globs into that shell.
        command.arg(format!("sh -c {}", shell_quote(BOOTSTRAP)));
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("could not start ssh")?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, events) = bounded(256);
        let (stop, shutdown) = bounded(1);
        let remote = Self {
            child,
            events,
            stop,
        };
        thread::spawn(move || {
            if let Err(error) = receive(input, BufReader::new(output), request, &sender, shutdown) {
                let _ = sender.send(Message::Failed(error));
            }
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if stopping.load(Ordering::Relaxed) {
                bail!("remote startup interrupted");
            }
            if Instant::now() > deadline {
                bail!("remote watcher startup timed out");
            }
            match remote.events.recv_timeout(Duration::from_millis(25)) {
                Ok(Message::Ready) => return Ok(remote),
                Ok(Message::Failed(error)) => return Err(error),
                Ok(Message::Change(_)) => bail!("remote sent a change before READY"),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    bail!("remote watcher disconnected during startup")
                }
            }
        }
    }

    pub fn try_recv(&self) -> Result<Option<WatchEvent>> {
        match self.events.try_recv() {
            Ok(Message::Change(event)) => Ok(Some(event)),
            Ok(Message::Failed(error)) => Err(error),
            Ok(Message::Ready) => bail!("duplicate remote READY message"),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => bail!("remote watcher disconnected"),
        }
    }
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        // Closing control stdin lets the helper exit and the shell remove its
        // private directory. Kill the local SSH client if graceful shutdown stalls.
        let _ = self.stop.try_send(());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn receive(
    mut input: ChildStdin,
    mut output: impl BufRead,
    request: WatchRequest,
    sender: &Sender<Message>,
    shutdown: Receiver<()>,
) -> Result<()> {
    // Bound startup output too: a noisy login shell must not allocate indefinitely.
    let mut greeting = Vec::new();
    std::io::Read::take(output.by_ref(), 1024).read_until(b'\n', &mut greeting)?;
    let greeting = std::str::from_utf8(&greeting).context("invalid SSH bootstrap greeting")?;
    let fields: Vec<_> = greeting.split_whitespace().collect();
    if fields.len() != 3 || fields[0] != "STALKER-BOOTSTRAP-1" {
        bail!(
            "invalid SSH bootstrap greeting {greeting:?}; remote shell startup files must keep stdout clean"
        );
    }
    let targets = platform_targets(fields[1], fields[2])?;
    let helper = targets.iter().find_map(|target| HELPERS.iter()
        .find(|(embedded, _)| embedded == target).map(|(_, bytes)| *bytes))
        .with_context(|| format!("no embedded helper for {} {}; build with `cargo xtask build` (or `cargo xtask build --native` for native development)", fields[1], fields[2]))?;
    writeln!(input, "{}\n{:x}", helper.len(), Sha256::digest(helper))?;
    input.write_all(helper).context("uploading remote helper")?;
    input.flush()?;
    match read_frame::<ServerMessage>(&mut output)?
        .context("remote helper exited before its handshake; check SSH stderr")?
    {
        ServerMessage::Hello { version } if version == protocol::VERSION => {}
        ServerMessage::Hello { version } => bail!(
            "remote protocol version mismatch: expected {}, got {version}",
            protocol::VERSION
        ),
        ServerMessage::Error(error) => bail!("remote watcher: {error}"),
        message => bail!("unexpected remote handshake: {message:?}"),
    }
    write_frame(&mut input, &request)?;
    thread::spawn(move || {
        loop {
            match shutdown.recv_timeout(Duration::from_secs(10)) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let _ = write_frame(&mut input, &ControlMessage::Stop);
                    return;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if write_frame(&mut input, &ControlMessage::Ping).is_err() {
                        return;
                    }
                }
            }
        }
    });
    loop {
        let message = match read_frame::<ServerMessage>(&mut output)?
            .context("SSH connection to remote watcher closed")?
        {
            ServerMessage::Ready => Message::Ready,
            ServerMessage::Change(event) => Message::Change(event.into()),
            ServerMessage::Error(error) => bail!("remote watcher: {error}"),
            ServerMessage::Hello { .. } => bail!("duplicate remote handshake"),
        };
        if sender.send(message).is_err() {
            return Ok(());
        }
    }
}

fn platform_targets(os: &str, arch: &str) -> Result<&'static [&'static str]> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => {
            Ok(&["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"])
        }
        ("Linux", "aarch64" | "arm64") => {
            Ok(&["aarch64-unknown-linux-musl", "aarch64-unknown-linux-gnu"])
        }
        ("Darwin", "x86_64" | "amd64") => Ok(&["x86_64-apple-darwin"]),
        ("Darwin", "arm64" | "aarch64") => Ok(&["aarch64-apple-darwin"]),
        _ => bail!(
            "unsupported remote platform: {os} {arch}; supported: Linux/macOS on x86-64/ARM64"
        ),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_preserves_the_fixed_script() {
        let output = Command::new("sh")
            .args(["-c", &format!("printf %s {}", shell_quote(BOOTSTRAP))])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, BOOTSTRAP.as_bytes());
    }

    #[test]
    fn platforms_are_explicit_and_do_not_guess_unknown_architectures() {
        assert_eq!(
            platform_targets("Darwin", "arm64").unwrap(),
            &["aarch64-apple-darwin"]
        );
        assert!(platform_targets("Linux", "riscv64").is_err());
        assert!(platform_targets("FreeBSD", "amd64").is_err());
    }
}

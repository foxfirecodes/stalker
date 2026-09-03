//! Child-process spawning, output streaming, and shutdown.

use std::{
    collections::HashMap,
    io::Read,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::SystemTime,
};

use crossbeam_channel::Sender;

use crate::{
    CommandId, CommandSpec, OutputStream, RunEvent, RunEventKind, RunId, RunStatus, RunTrigger,
};

/// Starts commands and publishes their lifecycle and output events.
///
/// One worker thread owns each child. Its stdout and stderr readers feed one
/// channel, so events preserve the order in which this process receives them.
/// The operating system does not expose a total order between the two pipes.
#[derive(Clone)]
pub struct ChildRunner {
    events: Sender<RunEvent>,
    raw_output: bool,
    active: Arc<Mutex<HashMap<CommandId, ActiveProcess>>>,
    stopping: Arc<AtomicBool>,
}

#[derive(Debug)]
struct ActiveProcess {
    pid: Option<u32>,
}

impl ChildRunner {
    /// When `raw_output` is enabled, child stdout and stderr inherit Stalker's
    /// terminal instead of being captured and forwarded through the event bus.
    /// This lets terminal-aware programs retain ANSI color and other TTY output.
    pub fn new(events: Sender<RunEvent>, raw_output: bool) -> Self {
        Self {
            events,
            raw_output,
            active: Arc::new(Mutex::new(HashMap::new())),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Spawn a command on a worker thread.
    ///
    /// `start` returns once the worker has been created. Spawn errors are sent
    /// as `RunEventKind::SpawnFailed`, which lets the scheduler stay alive and
    /// retry on a later filesystem change.
    pub fn start(&self, spec: CommandSpec, run_id: RunId, trigger: RunTrigger) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        // Reserve the slot before creating the worker. Shutdown can now wait
        // for a worker still between scheduling and `Command::spawn`.
        self.active
            .lock()
            .expect("active process lock poisoned")
            .insert(spec.id.clone(), ActiveProcess { pid: None });
        let runner = self.clone();
        thread::spawn(move || runner.run(spec, run_id, trigger));
    }

    /// Stop every active child. On Unix each child runs in its own process
    /// group, so this also stops descendants such as test runners.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        self.signal_active_processes(15);
    }

    /// Force-stop every active child group after a graceful shutdown period.
    pub fn force_shutdown(&self) {
        self.signal_active_processes(9);
    }

    fn signal_active_processes(&self, signal: i32) {
        let processes: Vec<ActiveProcess> = self
            .active
            .lock()
            .expect("active process lock poisoned")
            .values()
            .map(|process| ActiveProcess { pid: process.pid })
            .collect();

        for process in processes {
            if let Some(pid) = process.pid {
                signal_process(pid, signal);
            }
        }
    }

    /// Whether at least one child still owns an active process slot.
    pub fn is_active(&self) -> bool {
        !self
            .active
            .lock()
            .expect("active process lock poisoned")
            .is_empty()
    }

    fn run(&self, spec: CommandSpec, run_id: RunId, trigger: RunTrigger) {
        if self.stopping.load(Ordering::Acquire) {
            self.remove_active(&spec.id);
            return;
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null());
        if self.raw_output {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        configure_process_group(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.remove_active(&spec.id);
                self.send(
                    &spec.id,
                    run_id,
                    RunEventKind::SpawnFailed {
                        message: error.to_string(),
                    },
                );
                return;
            }
        };

        let pid = child.id();
        let should_stop = {
            let mut active = self.active.lock().expect("active process lock poisoned");
            if let Some(process) = active.get_mut(&spec.id) {
                process.pid = Some(pid);
            }
            self.stopping.load(Ordering::Acquire)
        };
        if should_stop {
            signal_process(pid, 15);
        }

        self.send(&spec.id, run_id, RunEventKind::Started { trigger });

        let readers = if self.raw_output {
            None
        } else {
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            Some((
                spawn_reader(
                    stdout,
                    OutputStream::Stdout,
                    self.events.clone(),
                    spec.id.clone(),
                    run_id,
                ),
                spawn_reader(
                    stderr,
                    OutputStream::Stderr,
                    self.events.clone(),
                    spec.id.clone(),
                    run_id,
                ),
            ))
        };

        let status = child.wait();
        // Pipes close when the child exits. Joining makes every output event
        // arrive before Finished, while readers stream output during the run.
        if let Some((stdout_reader, stderr_reader)) = readers {
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
        }

        self.remove_active(&spec.id);

        self.send(
            &spec.id,
            run_id,
            RunEventKind::Finished {
                status: status_to_run_status(status),
            },
        );
    }

    fn send(&self, command_id: &CommandId, run_id: RunId, kind: RunEventKind) {
        // A closed event bus means the application is already exiting.
        let _ = self.events.send(RunEvent {
            command_id: command_id.clone(),
            run_id,
            kind,
            timestamp: SystemTime::now(),
        });
    }

    fn remove_active(&self, command_id: &CommandId) {
        self.active
            .lock()
            .expect("active process lock poisoned")
            .remove(command_id);
    }
}

fn spawn_reader<R>(
    mut reader: R,
    stream: OutputStream,
    events: Sender<RunEvent>,
    command_id: CommandId,
    run_id: RunId,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0; 8 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(length) => {
                    if events
                        .send(RunEvent {
                            command_id: command_id.clone(),
                            run_id,
                            kind: RunEventKind::Output {
                                stream: stream.clone(),
                                bytes: buffer[..length].to_vec(),
                            },
                            timestamp: SystemTime::now(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                // A read error means this stream is finished. The child wait
                // still reports the process outcome.
                Err(_) => return,
            }
        }
    })
}

fn status_to_run_status(status: std::io::Result<ExitStatus>) -> RunStatus {
    match status {
        Ok(status) => exit_status_to_run_status(status),
        // `wait` errors are rare, but the run must still complete so the
        // scheduler cannot become stuck in Running.
        Err(_) => RunStatus::Signal("UNKNOWN".to_owned()),
    }
}

#[cfg(unix)]
fn exit_status_to_run_status(status: ExitStatus) -> RunStatus {
    use std::os::unix::process::ExitStatusExt;

    match status.signal() {
        Some(signal) => RunStatus::Signal(signal_name(signal).to_owned()),
        None => RunStatus::ExitCode(status.code().unwrap_or(-1)),
    }
}

#[cfg(not(unix))]
fn exit_status_to_run_status(status: ExitStatus) -> RunStatus {
    RunStatus::ExitCode(status.code().unwrap_or(-1))
}

#[cfg(unix)]
fn signal_name(signal: i32) -> &'static str {
    match signal {
        2 => "SIGINT",
        15 => "SIGTERM",
        9 => "SIGKILL",
        _ => "SIGNAL",
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `pre_exec` runs in the freshly forked child before exec. Calling
    // `setpgid(0, 0)` only changes that child's process group.
    unsafe {
        command.pre_exec(|| {
            if setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_: &mut Command) {}

#[cfg(unix)]
fn signal_process(pid: u32, signal: i32) {
    // Negative pid targets the process group created in `configure_process_group`.
    unsafe {
        let _ = kill(-(pid as i32), signal);
    }
}

#[cfg(not(unix))]
fn signal_process(_: u32, _: i32) {}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    fn spec(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec {
            id: CommandId("test".to_owned()),
            program: PathBuf::from(program),
            args: args.iter().map(OsString::from).collect(),
            cwd: std::env::current_dir().unwrap(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn emits_started_output_and_finished() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        ChildRunner::new(sender, false).start(spec("printf", &["hello"]), 7, RunTrigger::Initial);

        let events: Vec<_> = receiver.iter().take(3).collect();
        assert!(matches!(events[0].kind, RunEventKind::Started { .. }));
        assert!(
            matches!(events[1].kind, RunEventKind::Output { ref bytes, .. } if bytes == b"hello")
        );
        assert_eq!(
            events[2].kind,
            RunEventKind::Finished {
                status: RunStatus::ExitCode(0)
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_terminates_the_active_process_group() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let runner = ChildRunner::new(sender, false);
        runner.start(spec("sleep", &["10"]), 9, RunTrigger::Initial);

        let started = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(started.kind, RunEventKind::Started { .. }));
        std::thread::sleep(Duration::from_millis(100));
        runner.shutdown();

        let finished = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            finished.kind,
            RunEventKind::Finished {
                status: RunStatus::Signal("SIGTERM".to_owned())
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn forced_shutdown_kills_a_term_ignoring_child_group() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let runner = ChildRunner::new(sender, false);
        runner.start(
            spec("sh", &["-c", "trap '' TERM; while :; do sleep 1; done"]),
            10,
            RunTrigger::Initial,
        );

        let started = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(started.kind, RunEventKind::Started { .. }));
        std::thread::sleep(Duration::from_millis(100));
        runner.shutdown();
        runner.force_shutdown();

        let finished = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            finished.kind,
            RunEventKind::Finished {
                status: RunStatus::Signal("SIGKILL".to_owned())
            }
        );
    }

    #[test]
    fn reports_spawn_failure() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        ChildRunner::new(sender, false).start(
            spec("definitely-not-a-stalker-command", &[]),
            8,
            RunTrigger::Filesystem,
        );

        let event = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(event.kind, RunEventKind::SpawnFailed { .. }));
    }
}

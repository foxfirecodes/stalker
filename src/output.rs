//! Render structured run events for terminal output.

use std::io::{self, Stderr, Stdout, Write};

use crate::{RunEvent, RunEventKind, RunStatus, RunTrigger};

/// The terminal format used for command output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputMode {
    /// Forward child output without Stalker framing.
    #[default]
    Passthrough,
    /// Frame each run with machine-readable start and end lines.
    Markers,
}

/// Renders [`RunEvent`] values to separate output and diagnostic writers.
///
/// Both child streams share `output`: preserving the order in which the event
/// bus publishes them matters more than preserving their original file
/// descriptors. `diagnostics` is reserved for Stalker itself.
pub struct OutputRenderer<O, D> {
    mode: OutputMode,
    output: O,
    diagnostics: D,
    output_at_line_start: bool,
}

impl<O: Write, D: Write> OutputRenderer<O, D> {
    /// Create a renderer backed by caller-provided writers.
    ///
    /// Writer injection keeps rendering deterministic and easy to test.
    pub fn new(mode: OutputMode, output: O, diagnostics: D) -> Self {
        Self {
            mode,
            output,
            diagnostics,
            output_at_line_start: true,
        }
    }

    /// Render one event.
    pub fn render(&mut self, event: &RunEvent) -> io::Result<()> {
        match &event.kind {
            RunEventKind::Started { trigger } if self.mode == OutputMode::Markers => self
                .write_marker(&format!(
                    "@@stalker:start command={} run={} trigger={}@@\n",
                    event.command_id.0,
                    event.run_id,
                    trigger_name(trigger),
                )),
            RunEventKind::Output { bytes, .. } => self.write_child_output(bytes),
            RunEventKind::Finished { status } if self.mode == OutputMode::Markers => self
                .write_marker(&format!(
                    "@@stalker:end command={} run={} {}@@\n",
                    event.command_id.0,
                    event.run_id,
                    status_field(status),
                )),
            RunEventKind::SpawnFailed { message } => {
                writeln!(
                    self.diagnostics,
                    "stalker: command={} run={} failed to spawn: {message}",
                    event.command_id.0, event.run_id
                )?;
                self.diagnostics.flush()
            }
            RunEventKind::Started { .. } | RunEventKind::Finished { .. } => Ok(()),
        }
    }

    /// Return the writers after rendering has finished.
    pub fn into_inner(self) -> (O, D) {
        (self.output, self.diagnostics)
    }

    fn write_child_output(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        self.output.write_all(bytes)?;
        self.output.flush()?;
        self.output_at_line_start = bytes.last() == Some(&b'\n');
        Ok(())
    }

    fn write_marker(&mut self, marker: &str) -> io::Result<()> {
        if !self.output_at_line_start {
            self.output.write_all(b"\n")?;
        }
        self.output.write_all(marker.as_bytes())?;
        self.output.flush()?;
        self.output_at_line_start = true;
        Ok(())
    }
}

impl OutputRenderer<Stdout, Stderr> {
    /// Create a renderer that writes to the process terminal.
    pub fn terminal(mode: OutputMode) -> Self {
        Self::new(mode, io::stdout(), io::stderr())
    }
}

fn trigger_name(trigger: &RunTrigger) -> &'static str {
    match trigger {
        RunTrigger::Initial => "initial",
        RunTrigger::Filesystem => "filesystem",
    }
}

fn status_field(status: &RunStatus) -> String {
    match status {
        RunStatus::ExitCode(code) => format!("exit={code}"),
        RunStatus::Signal(signal) => format!("signal={signal}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, time::SystemTime};

    use super::*;
    use crate::{CommandId, OutputStream, RunEvent, RunEventKind, RunStatus, RunTrigger};

    fn event(run_id: u64, kind: RunEventKind) -> RunEvent {
        RunEvent {
            command_id: CommandId("default".into()),
            run_id,
            kind,
            timestamp: SystemTime::UNIX_EPOCH,
        }
    }

    fn render(mode: OutputMode, events: Vec<RunEvent>) -> (Vec<u8>, Vec<u8>) {
        let mut renderer =
            OutputRenderer::new(mode, Cursor::new(Vec::new()), Cursor::new(Vec::new()));
        for event in &events {
            renderer.render(event).unwrap();
        }
        let (output, diagnostics) = renderer.into_inner();
        (output.into_inner(), diagnostics.into_inner())
    }

    #[test]
    fn passthrough_preserves_received_stream_order_without_markers() {
        let (output, diagnostics) = render(
            OutputMode::Passthrough,
            vec![
                event(
                    1,
                    RunEventKind::Started {
                        trigger: RunTrigger::Initial,
                    },
                ),
                event(
                    1,
                    RunEventKind::Output {
                        stream: OutputStream::Stderr,
                        bytes: b"err ".to_vec(),
                    },
                ),
                event(
                    1,
                    RunEventKind::Output {
                        stream: OutputStream::Stdout,
                        bytes: b"out\n".to_vec(),
                    },
                ),
                event(
                    1,
                    RunEventKind::Finished {
                        status: RunStatus::ExitCode(0),
                    },
                ),
            ],
        );

        assert_eq!(output, b"err out\n");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn markers_frame_a_successful_initial_run() {
        let (output, diagnostics) = render(
            OutputMode::Markers,
            vec![
                event(
                    1,
                    RunEventKind::Started {
                        trigger: RunTrigger::Initial,
                    },
                ),
                event(
                    1,
                    RunEventKind::Output {
                        stream: OutputStream::Stdout,
                        bytes: b"hello\n".to_vec(),
                    },
                ),
                event(
                    1,
                    RunEventKind::Finished {
                        status: RunStatus::ExitCode(0),
                    },
                ),
            ],
        );

        assert_eq!(
            output,
            b"@@stalker:start command=default run=1 trigger=initial@@\nhello\n@@stalker:end command=default run=1 exit=0@@\n"
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn marker_mode_separates_unterminated_output_and_formats_signal_status() {
        let (output, _) = render(
            OutputMode::Markers,
            vec![
                event(
                    7,
                    RunEventKind::Started {
                        trigger: RunTrigger::Filesystem,
                    },
                ),
                event(
                    7,
                    RunEventKind::Output {
                        stream: OutputStream::Stderr,
                        bytes: b"stopping".to_vec(),
                    },
                ),
                event(
                    7,
                    RunEventKind::Finished {
                        status: RunStatus::Signal("SIGTERM".into()),
                    },
                ),
            ],
        );

        assert_eq!(
            output,
            b"@@stalker:start command=default run=7 trigger=filesystem@@\nstopping\n@@stalker:end command=default run=7 signal=SIGTERM@@\n"
        );
    }

    #[test]
    fn spawn_failures_are_diagnostics() {
        let (output, diagnostics) = render(
            OutputMode::Markers,
            vec![event(
                2,
                RunEventKind::SpawnFailed {
                    message: "missing program".into(),
                },
            )],
        );

        assert!(output.is_empty());
        assert_eq!(
            diagnostics,
            b"stalker: command=default run=2 failed to spawn: missing program\n"
        );
    }
}

//! `stalker` reruns configured commands after relevant filesystem changes.

pub mod config;
pub mod filter;
pub mod output;
pub mod runner;
pub mod scheduler;
pub mod watcher;

use std::{ffi::OsString, path::PathBuf, time::SystemTime};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommandId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub id: CommandId,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

pub type RunId = u64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunEventKind {
    Started {
        trigger: RunTrigger,
    },
    Output {
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Finished {
        status: RunStatus,
    },
    SpawnFailed {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunTrigger {
    Initial,
    Filesystem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunStatus {
    ExitCode(i32),
    Signal(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunEvent {
    pub command_id: CommandId,
    pub run_id: RunId,
    pub kind: RunEventKind,
    pub timestamp: SystemTime,
}

//! Versioned, length-prefixed messages. Paths preserve arbitrary Unix bytes.

use crate::watcher::WatchEvent;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePath(pub Vec<u8>);

impl WirePath {
    pub fn new(path: &Path) -> Self {
        Self(path.as_os_str().as_encoded_bytes().to_vec())
    }

    #[cfg(unix)]
    pub fn into_path(self) -> PathBuf {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        PathBuf::from(OsString::from_vec(self.0))
    }

    #[cfg(not(unix))]
    pub fn into_path(self) -> PathBuf {
        PathBuf::from(String::from_utf8_lossy(&self.0).into_owned())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WatchRequest {
    pub version: u32,
    pub cwd: Option<WirePath>,
    pub roots: Vec<WirePath>,
    pub includes: Vec<String>,
    pub ignores: Vec<String>,
    pub gitignore: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WireEvent {
    pub paths: Vec<WirePath>,
    pub is_directory: bool,
    pub is_broad: bool,
    pub is_overflow: bool,
}

impl From<WatchEvent> for WireEvent {
    fn from(event: WatchEvent) -> Self {
        Self {
            paths: event.paths.iter().map(|path| WirePath::new(path)).collect(),
            is_directory: event.is_directory,
            is_broad: event.is_broad,
            is_overflow: event.is_overflow,
        }
    }
}

impl From<WireEvent> for WatchEvent {
    fn from(event: WireEvent) -> Self {
        Self {
            paths: event.paths.into_iter().map(WirePath::into_path).collect(),
            is_directory: event.is_directory,
            is_broad: event.is_broad,
            is_overflow: event.is_overflow,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Hello { version: u32 },
    Ready,
    Change(WireEvent),
    Error(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ControlMessage {
    Ping,
    Stop,
}

pub fn write_frame(writer: &mut impl Write, message: &impl Serialize) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME {
        bail!("protocol frame exceeds {MAX_FRAME} bytes");
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<Option<T>> {
    let mut size = [0; 4];
    loop {
        match reader.read(&mut size[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    reader
        .read_exact(&mut size[1..])
        .context("truncated protocol frame header")?;
    let size = u32::from_be_bytes(size) as usize;
    if size > MAX_FRAME {
        bail!("protocol frame exceeds {MAX_FRAME} bytes");
    }
    let mut payload = vec![0; size];
    reader
        .read_exact(&mut payload)
        .context("truncated protocol frame")?;
    Ok(Some(
        serde_json::from_slice(&payload).context("invalid protocol message")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncated_and_oversized_frames() {
        assert!(read_frame::<ServerMessage>(&mut &b"\0\0"[..]).is_err());
        let header = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(read_frame::<ServerMessage>(&mut &header[..]).is_err());
        assert!(read_frame::<ServerMessage>(&mut &b"\0\0\0\x05abc"[..]).is_err());
        assert!(
            read_frame::<ServerMessage>(&mut &b""[..])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn event_paths_round_trip_non_utf8_newlines_and_shell_characters() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let path = PathBuf::from(OsString::from_vec(b"/tmp/\xff\n'$(false)".to_vec()));
        let event = WatchEvent {
            paths: vec![path.clone()],
            is_directory: false,
            is_broad: false,
            is_overflow: false,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &ServerMessage::Change(event.into())).unwrap();
        let Some(ServerMessage::Change(event)) = read_frame(&mut bytes.as_slice()).unwrap() else {
            panic!()
        };
        assert_eq!(WatchEvent::from(event).paths, vec![path]);
    }
}

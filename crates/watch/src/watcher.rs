//! Native filesystem watcher setup and normalized watch events.

use std::{
    error::Error,
    fmt, fs,
    path::{Component, PathBuf},
};

use crossbeam_channel::{Receiver, TryRecvError};
use notify::{Event, EventKind, RecursiveMode, Watcher};

#[cfg(target_os = "macos")]
type PlatformWatcher = notify::FsEventWatcher;

#[cfg(not(target_os = "macos"))]
type PlatformWatcher = notify::RecommendedWatcher;

/// A normalized notification for the scheduler and path filter.
///
/// `is_directory` is a hint, not a filesystem query contract: removed paths no
/// longer exist, so Notify's event kind takes precedence when it knows the
/// answer. `is_broad` means that a directory-level event may describe changed
/// descendants. `is_overflow` covers Notify's rescan flag and callback errors;
/// both require one conservative follow-up run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchEvent {
    pub paths: Vec<PathBuf>,
    pub is_directory: bool,
    pub is_broad: bool,
    pub is_overflow: bool,
}

impl WatchEvent {
    fn overflow(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            is_directory: true,
            is_broad: true,
            is_overflow: true,
        }
    }
}

/// A failure that prevents Stalker from keeping its native watcher alive.
#[derive(Debug)]
pub struct WatcherError {
    operation: &'static str,
    source: Box<dyn Error + Send + Sync>,
}

impl WatcherError {
    fn new(operation: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            operation,
            source: Box::new(source),
        }
    }
}

impl fmt::Display for WatcherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "could not {}: {}", self.operation, self.source)
    }
}

impl Error for WatcherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Owns the operating-system watcher and receives its normalized events.
///
/// Keep this value alive for as long as the event receiver is used. Watch setup
/// failures are returned immediately; callback errors are represented as
/// overflow events so the scheduler can run once more without trying to repair
/// or scan the tree.
pub struct NativeWatcher {
    watcher: PlatformWatcher,
    receiver: Receiver<notify::Result<Event>>,
    roots: Vec<PathBuf>,
}

impl NativeWatcher {
    /// Canonicalizes and recursively registers every root.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self, WatcherError> {
        let roots = canonicalize_roots(roots)?;
        let (sender, receiver) = crossbeam_channel::unbounded();
        let handler = move |event| {
            let _ = sender.send(event);
        };

        // Do not use `recommended_watcher` on macOS. Its selection can change
        // with Notify features, while Stalker explicitly promises FSEvents.
        #[cfg(target_os = "macos")]
        let mut watcher = PlatformWatcher::new(handler, notify::Config::default())
            .map_err(|error| WatcherError::new("create filesystem watcher", error))?;

        #[cfg(not(target_os = "macos"))]
        let mut watcher = notify::recommended_watcher(handler)
            .map_err(|error| WatcherError::new("create filesystem watcher", error))?;

        for root in &roots {
            let registration_root = if root.is_file() {
                root.parent().ok_or_else(|| {
                    WatcherError::new(
                        "find watch file parent",
                        std::io::Error::other("watch file has no parent directory"),
                    )
                })?
            } else {
                root.as_path()
            };
            watcher
                .watch(registration_root, RecursiveMode::Recursive)
                .map_err(|error| WatcherError::new("watch configured path", error))?;
        }

        Ok(Self {
            watcher,
            receiver,
            roots,
        })
    }

    /// Blocks until Notify sends an event. A disconnected callback is fatal.
    pub fn recv(&self) -> Result<WatchEvent, WatcherError> {
        loop {
            let event = self
                .receiver
                .recv()
                .map_err(|error| WatcherError::new("receive filesystem events", error))?;
            if let Some(event) = normalize_notify_result(event) {
                return Ok(event);
            }
        }
    }

    /// Returns an event if one is ready without blocking.
    pub fn try_recv(&self) -> Result<Option<WatchEvent>, WatcherError> {
        loop {
            match self.receiver.try_recv() {
                Ok(event) => {
                    if let Some(event) = normalize_notify_result(event) {
                        return Ok(Some(event));
                    }
                }
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    return Err(WatcherError::new(
                        "receive filesystem events",
                        TryRecvError::Disconnected,
                    ));
                }
            }
        }
    }

    /// The canonical roots passed to the backend.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The backend selected for this platform. Kept for diagnostics and tests.
    pub fn backend_kind(&self) -> notify::WatcherKind {
        let _keep_alive = &self.watcher;
        PlatformWatcher::kind()
    }
}

fn canonicalize_roots(
    roots: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<PathBuf>, WatcherError> {
    let mut canonical = Vec::new();
    for root in roots {
        let root = fs::canonicalize(&root)
            .map_err(|error| WatcherError::new("canonicalize watch root", error))?;
        if !canonical.contains(&root) {
            canonical.push(root);
        }
    }
    Ok(canonical)
}

fn normalize_notify_result(result: notify::Result<Event>) -> Option<WatchEvent> {
    match result {
        Ok(event) if event.need_rescan() => {
            Some(WatchEvent::overflow(normalize_paths(event.paths)))
        }
        Ok(event) if matches!(event.kind, EventKind::Access(_)) => None,
        Ok(event) => Some(normalize_event(event)),
        // Notify does not promise a recovered event after a callback error.
        // Schedule once instead of silently losing the change.
        Err(_) => Some(WatchEvent::overflow(Vec::new())),
    }
}

fn normalize_event(event: Event) -> WatchEvent {
    let paths = normalize_paths(event.paths);
    let is_directory = event_is_directory(&event.kind, &paths);
    let is_broad = paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other);

    WatchEvent {
        paths,
        is_directory,
        is_broad,
        is_overflow: false,
    }
}

fn event_is_directory(kind: &EventKind, paths: &[PathBuf]) -> bool {
    if matches!(
        kind,
        EventKind::Create(notify::event::CreateKind::Folder)
            | EventKind::Remove(notify::event::RemoveKind::Folder)
    ) {
        return true;
    }

    paths.iter().any(|path| path.is_dir())
}

fn normalize_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().map(normalize_path).collect()
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use notify::{Event, EventKind, event::Flag};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn rescan_events_become_overflow_events() {
        let event = Event::new(EventKind::Other)
            .add_path(PathBuf::from("src"))
            .set_flag(Flag::Rescan);

        assert_eq!(
            normalize_notify_result(Ok(event)).unwrap(),
            WatchEvent {
                paths: vec![PathBuf::from("src")],
                is_directory: true,
                is_broad: true,
                is_overflow: true,
            }
        );
    }

    #[test]
    fn notify_errors_become_overflow_events() {
        let event = normalize_notify_result(Err(notify::Error::generic("dropped events"))).unwrap();

        assert!(event.is_overflow);
        assert!(event.is_broad);
        assert!(event.paths.is_empty());
    }

    #[test]
    fn folder_events_are_directories_even_after_removal() {
        let event = Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
            .add_path(PathBuf::from("removed"));

        assert!(normalize_notify_result(Ok(event)).unwrap().is_directory);
    }

    #[test]
    fn broad_events_preserve_their_path() {
        let event = Event::new(EventKind::Any).add_path(PathBuf::from("src/./nested/../module"));
        let normalized = normalize_notify_result(Ok(event)).unwrap();

        assert_eq!(normalized.paths, vec![PathBuf::from("src/module")]);
        assert!(normalized.is_broad);
    }

    #[test]
    fn access_events_are_dropped() {
        let event = Event::new(EventKind::Access(notify::event::AccessKind::Any))
            .add_path(PathBuf::from("opened.rs"));

        assert_eq!(normalize_notify_result(Ok(event)), None);
    }

    #[test]
    fn canonicalize_roots_deduplicates_equivalent_paths() {
        let directory = tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let same_root = root.join(".");

        let roots = canonicalize_roots([root.clone(), same_root]).unwrap();
        assert_eq!(roots, vec![fs::canonicalize(root).unwrap()]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_fsevents() {
        let directory = tempdir().unwrap();
        let watcher = NativeWatcher::new([directory.path().to_path_buf()]).unwrap();

        assert_eq!(watcher.backend_kind(), notify::WatcherKind::Fsevent);
    }
}

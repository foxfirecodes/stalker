pub mod config;
pub mod filter;
pub mod protocol;
pub mod watcher;

use filter::{PathFilter, PathKind};
use watcher::WatchEvent;

pub fn event_is_relevant(event: &WatchEvent, filter: &PathFilter) -> bool {
    if event.is_overflow || (event.is_broad && event.paths.is_empty()) {
        return true;
    }
    let kind = if event.is_directory || event.is_broad {
        PathKind::Directory
    } else {
        PathKind::File
    };
    event.paths.iter().any(|path| filter.accepts(path, kind))
}

//! Path normalization and include/ignore filtering.

use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::Gitignore;

use crate::config::WatchSpec;

/// Whether an event path represents a file or a broad directory notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathKind {
    File,
    Directory,
}

/// Matches normalized event paths against the configured include and ignore rules.
#[derive(Debug)]
pub struct PathFilter {
    cwd: PathBuf,
    roots: Option<Vec<PathBuf>>,
    includes: GlobSet,
    ignores: GlobSet,
    include_patterns: Vec<String>,
    ignore_patterns: Vec<String>,
    gitignore: Option<GitignoreRules>,
}

impl PathFilter {
    pub fn from_watch_spec(
        cwd: impl Into<PathBuf>,
        watch: &WatchSpec,
        use_gitignore: bool,
    ) -> Result<Self, globset::Error> {
        let cwd = cwd.into();
        Self::new_with_patterns(
            cwd.clone(),
            &watch.includes,
            &watch.ignores,
            watch.include_patterns.clone(),
            watch.ignore_patterns.clone(),
            Some(watch.roots.clone()),
            if use_gitignore {
                GitignoreRules::discover(&cwd)
            } else {
                None
            },
        )
    }

    /// Build a filter from compiled globs. Directory events stay conservative
    /// when original pattern text is unavailable.
    pub fn new(
        cwd: impl Into<PathBuf>,
        includes: &[Glob],
        ignores: &[Glob],
    ) -> Result<Self, globset::Error> {
        Self::new_with_patterns(cwd, includes, ignores, Vec::new(), Vec::new(), None, None)
    }

    fn new_with_patterns(
        cwd: impl Into<PathBuf>,
        includes: &[Glob],
        ignores: &[Glob],
        include_patterns: Vec<String>,
        ignore_patterns: Vec<String>,
        roots: Option<Vec<PathBuf>>,
        gitignore: Option<GitignoreRules>,
    ) -> Result<Self, globset::Error> {
        Ok(Self {
            cwd: cwd.into(),
            roots,
            includes: build_set(includes)?,
            ignores: build_set(ignores)?,
            include_patterns,
            ignore_patterns,
            gitignore,
        })
    }

    /// Return a path relative to `cwd`, without requiring that the path still
    /// exists. Events for paths outside `cwd` do not have a CWD-relative name.
    pub fn normalize(&self, path: &Path) -> Option<PathBuf> {
        normalize_path(&self.cwd, path)
    }

    /// Decide whether an event should schedule a run.
    pub fn accepts(&self, path: &Path, kind: PathKind) -> bool {
        if !self.is_under_watch_root(path, kind) {
            return false;
        }
        let Some(path) = self.normalize(path) else {
            // Include and explicit ignore globs are defined relative to the
            // working directory, as are repository .gitignore rules. An
            // external watch root has no meaningful name in that namespace,
            // so accepting it here makes `--watch /other/path` useful without
            // accidentally applying the CWD repository's rules to it.
            return true;
        };
        if self.gitignore.as_ref().is_some_and(|rules| {
            rules.is_ignored(&self.cwd.join(&path), kind == PathKind::Directory)
        }) {
            return false;
        }
        self.accepts_normalized(&path, kind)
    }

    fn is_under_watch_root(&self, path: &Path, kind: PathKind) -> bool {
        let Some(roots) = &self.roots else {
            return true;
        };
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            self.cwd.join(path)
        };
        roots.iter().any(|root| {
            absolute.starts_with(root)
                // A broad parent event can describe a changed configured root.
                || (kind == PathKind::Directory && root.starts_with(&absolute))
        })
    }

    /// Decide whether a path already relative to `cwd` should schedule a run.
    pub fn accepts_normalized(&self, path: &Path, kind: PathKind) -> bool {
        if self.ignores.is_match(path)
            || (kind == PathKind::Directory && self.directory_is_fully_ignored(path))
        {
            return false;
        }
        if self.includes.is_empty() || self.includes.is_match(path) {
            return true;
        }
        kind == PathKind::Directory && self.directory_may_contain_include(path)
    }

    fn directory_may_contain_include(&self, directory: &Path) -> bool {
        // A broad event for the working-directory root can contain every
        // configured include. FSEvents commonly reports this shape.
        if directory.as_os_str().is_empty() {
            return true;
        }
        // A filter created from only compiled globs has no safe way to inspect
        // their prefixes. Accepting broad events is correct, if less selective.
        if self.include_patterns.is_empty() {
            return true;
        }
        self.include_patterns
            .iter()
            .any(|pattern| pattern_may_match_below(pattern, directory))
    }

    fn directory_is_fully_ignored(&self, directory: &Path) -> bool {
        self.ignore_patterns
            .iter()
            .any(|pattern| pattern_ignores_all_below(pattern, directory))
    }
}

/// Matches .gitignore files from a repository root through an event's parent.
/// Rules nearer the event take precedence, mirroring Git's normal lookup.
#[derive(Debug)]
struct GitignoreRules {
    root: PathBuf,
}

impl GitignoreRules {
    fn discover(cwd: &Path) -> Option<Self> {
        let mut directory = cwd;
        loop {
            if directory.join(".git").is_dir() || directory.join(".git").is_file() {
                return Some(Self {
                    root: directory.to_owned(),
                });
            }
            directory = directory.parent()?;
        }
    }

    fn is_ignored(&self, path: &Path, is_directory: bool) -> bool {
        if path.starts_with(self.root.join(".git")) {
            return true;
        }
        if !path.starts_with(&self.root) {
            return false;
        }

        let mut decision = None;
        for directory in self.gitignore_directories(path) {
            let ignore_file = directory.join(".gitignore");
            if !ignore_file.is_file() {
                continue;
            }
            let (matcher, _) = Gitignore::new(ignore_file);
            if matcher_has_ignored_parent(&matcher, path) {
                return true;
            }
            let matched = matcher.matched(path, is_directory);
            if matched.is_ignore() {
                decision = Some(true);
            } else if matched.is_whitelist() {
                decision = Some(false);
            }
        }
        decision.unwrap_or(false)
    }

    fn gitignore_directories(&self, path: &Path) -> Vec<PathBuf> {
        let mut directories = Vec::new();
        let mut directory = path.parent();
        while let Some(current) = directory {
            if current.starts_with(&self.root) {
                directories.push(current.to_owned());
            }
            if current == self.root {
                break;
            }
            directory = current.parent();
        }
        directories.reverse();
        directories
    }
}

fn matcher_has_ignored_parent(matcher: &Gitignore, path: &Path) -> bool {
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if directory == matcher.path() {
            break;
        }
        if matcher.matched(directory, true).is_ignore() {
            return true;
        }
        parent = directory.parent();
    }
    false
}

/// Normalize a possibly-relative event path against `cwd` and return its
/// slash-neutral relative form. It deliberately does not canonicalize: rename
/// and remove events often refer to paths that no longer exist.
pub fn normalize_path(cwd: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let relative = absolute.strip_prefix(cwd).ok()?;
    lexical_normalize(relative)
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn build_set(globs: &[Glob]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder.add(glob.clone());
    }
    builder.build()
}

fn pattern_may_match_below(pattern: &str, directory: &Path) -> bool {
    let directory = path_text(directory);
    let pattern = pattern.trim_start_matches("./").trim_matches('/');
    let literal = pattern
        .split(['*', '?', '[', '{'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');

    // A wildcard before any path segment can match below every directory.
    if literal.is_empty() {
        return true;
    }
    same_or_ancestor(&directory, literal) || same_or_ancestor(literal, &directory)
}

fn pattern_ignores_all_below(pattern: &str, directory: &Path) -> bool {
    let pattern = pattern.trim_start_matches("./").trim_matches('/');
    let Some(prefix) = pattern.strip_suffix("/**") else {
        return false;
    };
    Glob::new(prefix)
        .ok()
        .is_some_and(|glob| glob.compile_matcher().is_match(directory))
}

fn path_text(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn same_or_ancestor(ancestor: &str, descendant: &str) -> bool {
    ancestor == descendant
        || (!ancestor.is_empty()
            && descendant
                .strip_prefix(ancestor)
                .is_some_and(|remainder| remainder.starts_with('/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::Glob;
    use std::{fs, path::PathBuf};
    use tempfile::tempdir;

    fn filter(includes: &[&str], ignores: &[&str]) -> PathFilter {
        let includes = includes
            .iter()
            .map(|pattern| Glob::new(pattern).unwrap())
            .collect::<Vec<_>>();
        let ignores = ignores
            .iter()
            .map(|pattern| Glob::new(pattern).unwrap())
            .collect::<Vec<_>>();
        PathFilter::new_with_patterns(
            PathBuf::from("/repo"),
            &includes,
            &ignores,
            includes.iter().map(|glob| glob.glob().to_owned()).collect(),
            ignores.iter().map(|glob| glob.glob().to_owned()).collect(),
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn ignores_win_over_includes() {
        let filter = filter(&["**/*.rs"], &["target/**"]);
        assert!(filter.accepts(Path::new("/repo/src/main.rs"), PathKind::File));
        assert!(!filter.accepts(Path::new("/repo/target/main.rs"), PathKind::File));
    }

    #[test]
    fn normalizes_relative_and_absolute_paths() {
        let filter = filter(&[], &[]);
        assert_eq!(
            filter.normalize(Path::new("src/../src/main.rs")),
            Some(PathBuf::from("src/main.rs"))
        );
        assert_eq!(filter.normalize(Path::new("/outside/main.rs")), None);
    }

    #[test]
    fn directory_events_are_conservative_for_matching_descendants() {
        let filter = PathFilter::new_with_patterns(
            PathBuf::from("/repo"),
            &[Glob::new("src/**/*.rs").unwrap()],
            &[Glob::new("target/**").unwrap()],
            vec!["src/**/*.rs".to_owned()],
            vec!["target/**".to_owned()],
            None,
            None,
        )
        .unwrap();
        assert!(filter.accepts(Path::new("/repo/src"), PathKind::Directory));
        assert!(!filter.accepts(Path::new("/repo/docs"), PathKind::Directory));
        assert!(!filter.accepts(Path::new("/repo/target"), PathKind::Directory));
    }

    #[test]
    fn working_directory_broad_event_can_match_an_included_descendant() {
        let filter = filter(&["src/**/*.rs"], &[]);

        assert!(filter.accepts(Path::new("/repo"), PathKind::Directory));
    }

    #[test]
    fn wildcard_ignored_directories_do_not_schedule_broad_events() {
        let filter = filter(&["**/*.rs"], &["**/target/**"]);

        assert!(!filter.accepts(Path::new("/repo/target"), PathKind::Directory));
        assert!(!filter.accepts(Path::new("/repo/nested/target"), PathKind::Directory));
    }

    #[test]
    fn file_watch_root_filters_sibling_events_but_accepts_its_parent_broad_event() {
        let filter = PathFilter::new_with_patterns(
            PathBuf::from("/repo"),
            &[],
            &[],
            Vec::new(),
            Vec::new(),
            Some(vec![PathBuf::from("/repo/tsconfig.json")]),
            None,
        )
        .unwrap();

        assert!(filter.accepts(Path::new("/repo/tsconfig.json"), PathKind::File));
        assert!(!filter.accepts(Path::new("/repo/other.json"), PathKind::File));
        assert!(filter.accepts(Path::new("/repo"), PathKind::Directory));
    }

    #[test]
    fn external_watch_roots_bypass_cwd_gitignore_rules() {
        let repo = tempdir().unwrap();
        let external = tempdir().unwrap();
        let watched_file = external.path().join("gfx");
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join(".gitignore"), "*\n").unwrap();
        fs::write(&watched_file, "#!/bin/sh\n").unwrap();
        let filter = PathFilter::new_with_patterns(
            repo.path(),
            &[],
            &[],
            Vec::new(),
            Vec::new(),
            Some(vec![watched_file.clone()]),
            GitignoreRules::discover(repo.path()),
        )
        .unwrap();

        assert!(filter.accepts(&watched_file, PathKind::File));
    }

    #[test]
    fn repository_and_nested_gitignore_rules_filter_events() {
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::create_dir(repo.path().join("src")).unwrap();
        fs::write(repo.path().join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(repo.path().join("src/.gitignore"), "!keep.log\n").unwrap();
        let filter = PathFilter::new_with_patterns(
            repo.path(),
            &[],
            &[],
            Vec::new(),
            Vec::new(),
            None,
            GitignoreRules::discover(repo.path()),
        )
        .unwrap();

        assert!(!filter.accepts(&repo.path().join("target/output.o"), PathKind::File));
        assert!(!filter.accepts(&repo.path().join("error.log"), PathKind::File));
        assert!(filter.accepts(&repo.path().join("src/keep.log"), PathKind::File));
        assert!(!filter.accepts(&repo.path().join(".git/HEAD"), PathKind::File));
    }
}

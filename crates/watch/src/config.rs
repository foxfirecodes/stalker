use anyhow::{Context, Result, bail};
use globset::Glob;
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

/// Filesystem roots and matching rules shared by all commands in the MVP.
#[derive(Debug)]
pub struct WatchSpec {
    /// Roots in the watched filesystem; canonicalized before native watching.
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

pub fn canonical_directory(path: &Path) -> Result<PathBuf> {
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

pub fn canonical_watch_path(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .with_context(|| format!("could not resolve watch path {}", path.display()))
}

pub fn compile_globs(patterns: &[String], kind: &str) -> Result<Vec<Glob>> {
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
pub fn compile_ignore_globs(patterns: &[String]) -> Result<Vec<Glob>> {
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

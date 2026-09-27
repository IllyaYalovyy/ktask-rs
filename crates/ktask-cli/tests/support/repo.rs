//! Scratch projects for the tests: a directory to work in and git repositories inside it.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::support::{Result, Sandbox};

/// A scratch directory, canonical so that it can be compared with what the binary prints.
pub(crate) fn scratch() -> Result<(TempDir, PathBuf)> {
    let dir = TempDir::new()?;
    let path = std::fs::canonicalize(dir.path())?;
    Ok((dir, path))
}

/// A new git repository called `name` inside `parent`.
pub(crate) fn git_repository(sandbox: &Sandbox, parent: &Path, name: &str) -> Result<PathBuf> {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir)?;
    let mut command = Command::new("git");
    command.args(["init", "--quiet"]);
    let status = sandbox.isolate(&mut command, &dir).status()?;
    assert!(status.success());
    Ok(dir)
}

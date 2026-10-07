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
    write_instruction_files(&dir.join("docs"))?;
    let exclude = dir.join(".git").join("info").join("exclude");
    std::fs::create_dir_all(exclude.parent().ok_or("no info directory")?)?;
    std::fs::write(exclude, "/docs/\n")?;
    Ok(dir)
}

/// The instruction files every agent prompt opens with, and what each one holds: the vision,
/// then one file per role.
pub(crate) const INSTRUCTION_FILES: [(&str, &str); 5] = [
    (
        "VISION.md",
        "# Vision\nThe vision of the scratch project.\n",
    ),
    ("CODER.md", "# Coder\nThe coder's own instructions.\n"),
    (
        "REVIEWER.md",
        "# Reviewer\nThe reviewer's own instructions.\n",
    ),
    ("TESTER.md", "# Tester\nThe tester's own instructions.\n"),
    (
        "RESOLVER.md",
        "# Resolver\nThe resolver's own instructions.\n",
    ),
];

/// Writes every instruction file into `dir`, creating it when it does not exist yet.
pub(crate) fn write_instruction_files(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, text) in INSTRUCTION_FILES {
        std::fs::write(dir.join(name), text)?;
    }
    Ok(())
}

//! What the build records about itself: its channel and the commit it was built from.
//! Shared by `build.rs`, which writes it into the binary, and by the tests that check it
//! against real repositories.

use std::path::Path;
use std::process::Command;

/// The channel a build was asked for through `KTASK_RS_CHANNEL`: `dev` when it is unset,
/// `user` only when the installer sets it, and an error for anything else.
pub(crate) fn channel(requested: Option<&str>) -> Result<&'static str, String> {
    match requested {
        None | Some("dev") => Ok("dev"),
        Some("user") => Ok("user"),
        Some(other) => Err(format!(
            "KTASK_RS_CHANNEL must be `dev` or `user`, not {other:?}"
        )),
    }
}

/// The commit a tree was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Revision {
    /// The short commit hash of `HEAD`, or `unknown` when there is no git history to ask.
    pub(crate) commit: String,
    /// Whether the tree had changes that are not committed, new files included.
    pub(crate) dirty: bool,
}

impl Revision {
    /// The revision of the git working tree `dir` is in.
    pub(crate) fn read(dir: &Path) -> Self {
        let commit = git(dir, &["rev-parse", "--short", "HEAD"])
            .map_or_else(|| "unknown".to_owned(), |text| text.trim().to_owned());
        let dirty = git(dir, &["status", "--porcelain"]).is_some_and(|text| !text.is_empty());
        Self { commit, dirty }
    }

    /// The commit as the version line shows it: `-dirty` is appended for a dirty tree.
    pub(crate) fn label(&self) -> String {
        if self.dirty {
            format!("{}-dirty", self.commit)
        } else {
            self.commit.clone()
        }
    }
}

/// What `git` prints with `args` run in `dir`, or `None` when it cannot be run or fails.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

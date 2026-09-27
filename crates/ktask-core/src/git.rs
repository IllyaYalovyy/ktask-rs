//! What the tool needs to know from git.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

/// Why git could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    message: String,
}

impl GitError {
    /// An error described by `message`, which names what failed and why.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for GitError {}

/// Port: the git repository a directory belongs to.
pub trait Git {
    /// The canonical root of the working tree that contains `dir`, or `None` when `dir` is
    /// not inside a git repository.
    ///
    /// # Errors
    ///
    /// Fails when git cannot be run or cannot tell.
    fn work_tree_root(&self, dir: &Path) -> Result<Option<PathBuf>, GitError>;
}

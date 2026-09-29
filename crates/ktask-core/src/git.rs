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

/// What bringing a directory up to date with a tracked branch by rebase found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRebase {
    /// Already up to date: nothing to bring in.
    UpToDate,
    /// Rebased in this many commits.
    TookIn(u64),
}

/// Why bringing a directory up to date with a tracked branch by rebase was refused, or could
/// not be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRebaseError {
    /// The directory holds changes that have not been committed: the status listing, as git
    /// itself printed it, trimmed.
    UncommittedChanges(String),
    /// The remote could not be reached: why, as git itself said.
    RemoteUnreachable(String),
    /// Rebasing conflicted in these files. The rebase was undone before this was returned: the
    /// directory is exactly as it was.
    Conflict(Vec<String>),
    /// It could not be done, for some other reason.
    Failed(String),
}

/// Why committing everything changed in a directory was refused, or could not be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitAllError {
    /// The directory has changes to commit, but no identity is configured to commit them
    /// under.
    IdentityNotConfigured,
    /// It could not be done, for some other reason.
    Failed(String),
}

/// Why pushing a directory's `HEAD` to a tracked branch, and confirming it landed, was
/// refused, or could not be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    /// The push was rejected: the tracked branch has moved on since the commit being pushed
    /// was made.
    Rejected,
    /// It could not be pushed, or the push could not be confirmed against the remote, for some
    /// other reason.
    Failed(String),
}

/// Port: the git repository a directory belongs to, and the operations the tool's own steps
/// need from it.
pub trait Git {
    /// The canonical root of the working tree that contains `dir`, or `None` when `dir` is
    /// not inside a git repository.
    ///
    /// # Errors
    ///
    /// Fails when git cannot be run or cannot tell.
    fn work_tree_root(&self, dir: &Path) -> Result<Option<PathBuf>, GitError>;

    /// Whether `branch` exists on `remote`, checked live against the remote itself — never
    /// merely a locally cached remote-tracking ref, so this tells the truth even before
    /// anything has ever been fetched. `false` for any reason `remote`/`branch` cannot be
    /// confirmed: no such remote, no such branch on it, or the remote could not be reached.
    ///
    /// # Errors
    ///
    /// Fails only when git itself could not be run at all.
    fn remote_branch_exists(
        &self,
        dir: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<bool, GitError>;

    /// Brings `dir` up to date with `branch` on `remote` by rebase: refused when `dir` holds
    /// uncommitted changes or `remote` cannot be reached; rebases onto the branch when
    /// anything new was fetched, undoing the rebase and naming every file it conflicted in
    /// when it did.
    ///
    /// # Errors
    ///
    /// Fails when the sync is refused, conflicts, or could not be done.
    fn pull_rebase(
        &self,
        dir: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<PullRebase, PullRebaseError>;

    /// `dir`'s current commit, or `None` when it has none yet or git could not tell.
    fn head(&self, dir: &Path) -> Option<String>;

    /// Everything changed in `dir` since `start_commit`, committed or still sitting
    /// uncommitted in the working tree. Empty when git could not produce it.
    fn diff_since(&self, dir: &Path, start_commit: &str) -> String;

    /// Commits everything changed in `dir`, under whatever identity git is configured with
    /// there — author and committer both — with `message`. `None`, refusing nothing, when
    /// nothing had changed: no commit is made. The commit's short hash otherwise.
    ///
    /// # Errors
    ///
    /// Fails when there is something to commit but no identity is configured, or git itself
    /// refuses the commit.
    fn commit_all(&self, dir: &Path, message: &str) -> Result<Option<String>, CommitAllError>;

    /// Pushes `dir`'s `HEAD` to `branch` on `remote`, then confirms — checked live against the
    /// remote rather than any locally cached ref — that the branch's tip there is now that
    /// commit. The short hash pushed.
    ///
    /// # Errors
    ///
    /// Fails when the push is rejected, cannot be run, or the remote's tip does not turn out
    /// to match afterwards.
    fn push_and_confirm(&self, dir: &Path, remote: &str, branch: &str)
    -> Result<String, PushError>;
}

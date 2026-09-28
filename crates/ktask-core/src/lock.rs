//! Port: the lock a run holds for the whole of its project's queue, so that two runs of the
//! same project never overlap.

use std::error::Error;
use std::fmt;

/// Why the run lock could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLockError {
    /// A run is already in progress; the process id of the run holding the lock, when it
    /// could be read.
    InProgress(Option<u32>),
    /// The lock could not be used at all — its file could not be opened, read or written.
    Unusable(String),
}

impl fmt::Display for RunLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InProgress(Some(pid)) => {
                write!(f, "a run is already in progress: process {pid}")
            }
            Self::InProgress(None) => write!(f, "a run is already in progress"),
            Self::Unusable(message) => f.write_str(message),
        }
    }
}

impl Error for RunLockError {}

/// Port: the lock a run holds for the whole of its project's queue, so that two runs of the
/// same project never overlap. Held for as long as the run lasts; released, at the latest,
/// when the process holding it ends — including a kill — so an interrupted run never leaves
/// the lock stuck.
pub trait RunLock {
    /// Takes the lock, at once, without waiting.
    ///
    /// # Errors
    ///
    /// Fails when another run already holds the lock, naming its process when known, or when
    /// the lock cannot be used at all.
    fn acquire(&self) -> Result<(), RunLockError>;

    /// Whether a run currently holds the lock — without taking it, and without disturbing
    /// whoever holds it. Used to tell a task the journal still calls `running` apart: still
    /// truly running, or left behind by a run that is no longer alive.
    ///
    /// # Errors
    ///
    /// Fails when the lock cannot be used at all.
    fn in_progress(&self) -> Result<bool, RunLockError>;
}

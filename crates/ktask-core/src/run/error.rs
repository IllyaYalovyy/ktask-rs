//! Why a run could not proceed at all.

use std::error::Error;
use std::fmt;

use crate::{BeginAttemptError, JournalError, RecordReportError, RunLockError};

/// Why a run could not proceed at all — never for how an attempt itself ended, which is a
/// normal [`RunReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// Another run already holds the project's run lock.
    Locked(RunLockError),
    /// Some other failure — the journal could not be read or written.
    Other(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked(error) => error.fmt(f),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl Error for RunError {}

impl From<JournalError> for RunError {
    fn from(error: JournalError) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<BeginAttemptError> for RunError {
    fn from(error: BeginAttemptError) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<RecordReportError> for RunError {
    fn from(error: RecordReportError) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<RunLockError> for RunError {
    fn from(error: RunLockError) -> Self {
        Self::Locked(error)
    }
}

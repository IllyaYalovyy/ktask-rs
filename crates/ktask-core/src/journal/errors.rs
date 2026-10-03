//! Why a command against the journal was refused: adding a task, cancelling, beginning or
//! retrying an attempt, or recording a report.

use std::error::Error;
use std::fmt;

use crate::{TaskId, TaskStatus};

use super::JournalError;

/// Why a task was not added at the place it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    /// The task the new one was to be placed next to does not exist.
    UnknownTask(TaskId),
    /// The task the new one was to be placed next to was cancelled.
    CancelledTask(TaskId),
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::CancelledTask(id) => write!(f, "task {id} is cancelled"),
        }
    }
}

impl Error for AppendError {}

/// Why [`super::Journal::append_events`] refused to append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendConflict {
    /// The journal already held more events than were read to decide the ones given: read
    /// it again, decide again, and retry.
    Conflict,
    /// The journal could not be written.
    Journal(JournalError),
}

impl fmt::Display for AppendConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => f.write_str("the journal has moved on since it was read"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AppendConflict {}

impl From<JournalError> for AppendConflict {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Why a task was not cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelError {
    /// There is no such task.
    UnknownTask(TaskId),
    /// The task was cancelled already.
    AlreadyCancelled(TaskId),
    /// The task is running: it must finish or be interrupted first.
    Running(TaskId),
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::AlreadyCancelled(id) => write!(f, "task {id} is already cancelled"),
            Self::Running(id) => write!(f, "task {id} is running"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for CancelError {}

impl From<JournalError> for CancelError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Why an attempt could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginAttemptError {
    /// There is no such task.
    UnknownTask(TaskId),
    /// The task is not pending, so it cannot be started: it is already running, or it is
    /// done, failed or cancelled.
    NotPending(TaskId),
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for BeginAttemptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::NotPending(id) => write!(f, "task {id} is not pending"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for BeginAttemptError {}

impl From<JournalError> for BeginAttemptError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Why a task could not be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryError {
    /// There is no such task.
    UnknownTask(TaskId),
    /// The task's own status is not one `retry` accepts: only `failed`, `failed-unknown` or
    /// `blocked` can be.
    NotRetryable {
        /// The task that cannot be retried.
        id: TaskId,
        /// Its current status.
        status: TaskStatus,
    },
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for RetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::NotRetryable { id, status } => write!(
                f,
                "task {id} is {status}: only a failed, failed-unknown or blocked task can be \
                 retried"
            ),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for RetryError {}

impl From<JournalError> for RetryError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Why a task was not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// There is no such task.
    UnknownTask(TaskId),
    /// The task's own status is not `blocked`: only a blocked task can be answered.
    NotBlocked {
        /// The task that cannot be answered.
        id: TaskId,
        /// Its current status.
        status: TaskStatus,
    },
    /// The answer is empty or only whitespace.
    EmptyAnswer,
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::NotBlocked { id, status } => {
                write!(
                    f,
                    "task {id} is {status}: only a blocked task can be answered"
                )
            }
            Self::EmptyAnswer => f.write_str("the answer is empty"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AnswerError {}

impl From<JournalError> for AnswerError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Why a task was not marked done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneError {
    /// There is no such task.
    UnknownTask(TaskId),
    /// The task's own status is not one `done` accepts: only `failed`, `failed-unknown` or
    /// `blocked` can be.
    NotDoneable {
        /// The task that cannot be marked done.
        id: TaskId,
        /// Its current status.
        status: TaskStatus,
    },
    /// The reason is empty or only whitespace.
    EmptyReason,
    /// The journal could not be used.
    Journal(JournalError),
}

/// Why a human task was not acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcknowledgeError {
    /// There is no such task.
    UnknownTask(TaskId),
    /// The task is not a pending human task.
    NotAcknowledgeable {
        /// The task that cannot be acknowledged.
        id: TaskId,
        /// Its kind.
        kind: crate::TaskKind,
        /// Its current status.
        status: TaskStatus,
    },
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for AcknowledgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::NotAcknowledgeable { id, kind, status } => write!(
                f,
                "task {id} is a {status} {kind} task: only a pending human task can be acknowledged"
            ),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AcknowledgeError {}

impl From<JournalError> for AcknowledgeError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl fmt::Display for DoneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::NotDoneable { id, status } => write!(
                f,
                "task {id} is {status}: only a failed, failed-unknown or blocked task can be \
                 marked done"
            ),
            Self::EmptyReason => f.write_str("the reason is empty"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for DoneError {}

impl From<JournalError> for DoneError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Why a report could not be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordReportError {
    /// No attempt numbered like this was ever started for this task.
    UnknownAttempt {
        /// The task the report named.
        task: TaskId,
        /// The attempt number the report named.
        number: u32,
    },
    /// This attempt was started, but is no longer running.
    AttemptEnded {
        /// The task the report named.
        task: TaskId,
        /// The attempt number the report named.
        number: u32,
    },
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for RecordReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAttempt { task, number } => {
                write!(f, "there is no attempt {number} of task {task}")
            }
            Self::AttemptEnded { task, number } => {
                write!(f, "attempt {number} of task {task} has ended")
            }
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for RecordReportError {}

impl From<JournalError> for RecordReportError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

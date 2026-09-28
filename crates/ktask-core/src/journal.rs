//! The journal: where a project's events and the tasks projected from them are kept.

use std::error::Error;
use std::fmt;
use std::time::SystemTime;

use crate::{Outcome, Placement, Task, TaskDraft, TaskId};

/// Why the journal could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalError {
    message: String,
}

impl JournalError {
    /// An error described by `message`, which names what failed and why.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for JournalError {}

/// Why a task was not appended to the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    /// The task the new one was to be placed next to does not exist.
    UnknownTask(TaskId),
    /// The task the new one was to be placed next to was cancelled.
    CancelledTask(TaskId),
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::CancelledTask(id) => write!(f, "task {id} is cancelled"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AppendError {}

impl From<JournalError> for AppendError {
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
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::AlreadyCancelled(id) => write!(f, "task {id} is already cancelled"),
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

/// Port: one project's journal. Every change is an event appended to it, and the tasks are
/// updated from that event in the same transaction.
pub trait Journal {
    /// Appends a task-added event for each of `drafts`, which the caller has validated, and
    /// puts the tasks in the queue in that order, together, at `placement` — before or after
    /// the task it names, or at the end — with the next numbers, all in one transaction.
    /// Numbers start at 1 and are never reused, and no other task's number changes. Returns
    /// the tasks as stored.
    ///
    /// # Errors
    ///
    /// Fails, recording nothing, when `placement` names a task that does not exist or was
    /// cancelled, or when the journal cannot be written.
    fn append_tasks(
        &self,
        drafts: &[TaskDraft],
        placement: Placement,
        at: SystemTime,
    ) -> Result<Vec<Task>, AppendError>;

    /// Appends a task-added event for `draft`, which the caller has validated, and puts the
    /// task at `placement` in the queue with the next number, in one transaction. Returns the
    /// task as stored.
    ///
    /// # Errors
    ///
    /// As [`Journal::append_tasks`].
    fn append_task(
        &self,
        draft: &TaskDraft,
        placement: Placement,
        at: SystemTime,
    ) -> Result<Task, AppendError> {
        let mut tasks = self.append_tasks(std::slice::from_ref(draft), placement, at)?;
        tasks
            .pop()
            .ok_or_else(|| JournalError::new("the journal stored no task").into())
    }

    /// Appends a task-cancelled event for the task numbered `id` and marks the task cancelled,
    /// in one transaction. The task stays in the journal and keeps its number.
    ///
    /// # Errors
    ///
    /// Fails, recording nothing, when there is no such task, when it is cancelled already, or
    /// when the journal cannot be written.
    fn cancel_task(&self, id: TaskId, at: SystemTime) -> Result<(), CancelError>;

    /// Every task, cancelled ones included, in queue order, with positions counting from 1.
    ///
    /// # Errors
    ///
    /// Fails when the journal cannot be read.
    fn tasks(&self) -> Result<Vec<Task>, JournalError>;

    /// Starts the next attempt at the pending task numbered `id`: records an attempt-started
    /// event and marks the task running, in one transaction. Returns the attempt's number,
    /// starting at 1 and never reused for this task.
    ///
    /// # Errors
    ///
    /// Fails, recording nothing, when there is no such task, when it is not pending, or when
    /// the journal cannot be written.
    fn begin_attempt(&self, id: TaskId, at: SystemTime) -> Result<u32, BeginAttemptError>;

    /// Records a report for attempt `number` of the task numbered `id`: one event carrying
    /// `outcome` and `reason`. A later report for the same running attempt is recorded the
    /// same way and stands as the current one; both stay in the journal.
    ///
    /// # Errors
    ///
    /// Fails, recording nothing, when no attempt numbered `number` was started for this task,
    /// when it was but has since ended, or when the journal cannot be written.
    fn record_report(
        &self,
        id: TaskId,
        number: u32,
        outcome: Outcome,
        reason: Option<&str>,
        at: SystemTime,
    ) -> Result<(), RecordReportError>;
}

/// Port: notices when a project's journal changes, so a frontend can show what another
/// process did to it without polling.
pub trait JournalWatch {
    /// Blocks the calling thread until the journal changes, then returns. Meant to be called
    /// again and again, from a dedicated thread, so that every change after the first is
    /// reported too.
    ///
    /// # Errors
    ///
    /// Fails when the watch cannot be kept up, and will not report further changes.
    fn wait(&self) -> Result<(), JournalError>;
}

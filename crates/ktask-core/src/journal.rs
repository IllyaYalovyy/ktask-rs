//! The journal: where a project's events and the tasks projected from them are kept.

use std::error::Error;
use std::fmt;
use std::time::{Duration, SystemTime};

use crate::{Outcome, Placement, TaskDraft, TaskId, TaskStatus};

/// One thing that happened to the queue: what [`Journal::events`] reads and
/// [`Journal::append_events`] writes. [`crate::queue_state`] is the one place that decides
/// what these mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A task was added.
    TaskAdded {
        /// The number it was given.
        id: TaskId,
        /// What it was written with.
        draft: TaskDraft,
        /// Where it was placed.
        placement: Placement,
        /// When.
        at: SystemTime,
    },
    /// A task was cancelled.
    TaskCancelled {
        /// The task's number.
        id: TaskId,
        /// When.
        at: SystemTime,
    },
    /// An attempt at a task began.
    AttemptStarted {
        /// The task attempted.
        id: TaskId,
        /// The attempt's number.
        number: u32,
        /// When.
        at: SystemTime,
    },
    /// An attempt started to run with a provider.
    AttemptRunning {
        /// The task attempted.
        id: TaskId,
        /// The attempt's number.
        number: u32,
        /// The provider it runs with.
        provider: String,
        /// When.
        at: SystemTime,
    },
    /// The agent reported an attempt's outcome.
    AttemptReported {
        /// The task attempted.
        id: TaskId,
        /// The attempt's number.
        number: u32,
        /// What the agent reported.
        outcome: Outcome,
        /// Why, when the outcome needs a reason.
        reason: Option<String>,
        /// When.
        at: SystemTime,
    },
    /// An attempt ended.
    AttemptEnded {
        /// The task attempted.
        id: TaskId,
        /// The attempt's number.
        number: u32,
        /// How long the provider ran.
        duration: Duration,
        /// The provider's exit code, or `None` when it was killed past its time limit.
        exit_code: Option<i32>,
        /// What the attempt, and so the task, ends at.
        status: TaskStatus,
        /// Why, when `status` is not `done`.
        reason: Option<String>,
        /// When.
        at: SystemTime,
    },
}

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

/// Why [`Journal::append_events`] refused to append.
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

/// What running an attempt produced, given to [`crate::attempt::end_attempt`].
#[derive(Debug, Clone, Copy)]
pub struct AttemptRun<'a> {
    /// How long the provider ran.
    pub duration: Duration,
    /// The provider's exit code, or `None` when it was killed past its time limit.
    pub exit_code: Option<i32>,
    /// What the attempt, and so the task, ends at.
    pub status: TaskStatus,
    /// Why, when `status` is not `done`.
    pub reason: Option<&'a str>,
}

/// How an attempt ended, as [`crate::attempt::last_attempt`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptEnd {
    /// How long the provider ran.
    pub duration: Duration,
    /// What the attempt ended at.
    pub status: TaskStatus,
    /// Why, when `status` is not `done`.
    pub reason: Option<String>,
}

/// A task's most recent attempt, as [`crate::attempt::last_attempt`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The attempt's number.
    pub number: u32,
    /// When it started.
    pub started_at: SystemTime,
    /// The provider it ran with, once [`crate::attempt::begin_attempt_running`] has recorded
    /// it.
    pub provider: Option<String>,
    /// How it ended, once [`crate::attempt::end_attempt`] has recorded it; `None` while it
    /// runs.
    pub ended: Option<AttemptEnd>,
}

/// Port: one project's journal. Every change is an event appended to it, and the queue's
/// state — tasks and attempts alike — is a projection [`crate::queue_state::QueueState`]
/// folds from those events; the journal itself decides nothing about what they mean.
pub trait Journal {
    /// Every event recorded for the queue, in the order they were appended.
    ///
    /// # Errors
    ///
    /// Fails when the journal cannot be read.
    fn events(&self) -> Result<Vec<Event>, JournalError>;

    /// Appends `events`, together, atomically, provided the journal still holds exactly
    /// `read` events — as many as [`Journal::events`] returned when they were decided.
    ///
    /// # Errors
    ///
    /// Refuses with [`AppendConflict::Conflict`], recording nothing, when the journal holds
    /// more than `read` events already: something else appended first, so the events given
    /// were decided against a queue that has since moved on — read it again, decide again,
    /// and retry. Fails with [`AppendConflict::Journal`] when the journal cannot be written.
    fn append_events(&self, events: &[Event], read: usize) -> Result<(), AppendConflict>;
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

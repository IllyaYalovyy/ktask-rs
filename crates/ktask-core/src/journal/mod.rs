//! The journal: where a project's events and the tasks projected from them are kept.

use std::error::Error;
use std::fmt;
use std::time::{Duration, SystemTime};

use crate::{Outcome, Placement, TaskDraft, TaskId, TaskStatus};

mod errors;

pub use errors::{
    AnswerError, AppendConflict, AppendError, BeginAttemptError, CancelError, DoneError,
    RecordReportError, RetryError,
};

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
        /// The project's commit `HEAD` right before this attempt began, so a later attempt's
        /// own prompt can show everything the task has changed since its first attempt
        /// started. `None` when it could not be captured.
        start_commit: Option<String>,
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
        /// The step that was open when this was reported — so a later step's own report of
        /// the same attempt never reads back as this one's. `None` when no step was open,
        /// which only happens ahead of the pipeline itself ever beginning one.
        step: Option<String>,
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
    /// A step of an attempt began.
    StepStarted {
        /// The task attempted.
        id: TaskId,
        /// The attempt's number.
        number: u32,
        /// The step's name.
        step: String,
        /// When.
        at: SystemTime,
    },
    /// A step of an attempt ended.
    StepEnded {
        /// The task attempted.
        id: TaskId,
        /// The attempt's number.
        number: u32,
        /// The step's name.
        step: String,
        /// How long the step ran.
        duration: Duration,
        /// The provider's exit code, or `None` when it was killed past its time limit.
        exit_code: Option<i32>,
        /// What the step, and so — when it is the last one run — the attempt, ends at.
        status: TaskStatus,
        /// Why, when `status` is not `done`.
        reason: Option<String>,
        /// The fine-grained outcome the agent itself reported for this step, distinct from
        /// `status` which collapses several outcomes into one; `None` for a step the tool
        /// records as already passed, which no agent ever reports an outcome for.
        reported: Option<Outcome>,
        /// When.
        at: SystemTime,
    },
    /// The sync or health-check gate ahead of a task's attempt refused it, leaving it `pending`.
    GateFailed {
        /// The task the gate was ahead of.
        id: TaskId,
        /// [`crate::SYNC_STEP`] or [`crate::HEALTH_CHECK_STEP`].
        step: String,
        /// What failed and what is expected, in the run's own words.
        reason: String,
        /// When.
        at: SystemTime,
    },
    /// A task that had ended `failed`, `failed-unknown` or `blocked` was sent back to
    /// `pending`, so the next run picks it up again; every earlier attempt stays in the
    /// journal, and the next one begins at the next number.
    TaskRetried {
        /// The task retried.
        id: TaskId,
        /// When.
        at: SystemTime,
    },
    /// A `blocked` task was given the answer to the question its attempt asked, and sent
    /// back to `pending` so the next attempt can carry both the question and this answer.
    TaskAnswered {
        /// The task answered.
        id: TaskId,
        /// The answer given.
        text: String,
        /// When.
        at: SystemTime,
    },
    /// A task that had ended `failed`, `failed-unknown` or `blocked` was marked `done` by the
    /// operator's own hand, with `reason` — work finished outside the tool.
    TaskDoneByUser {
        /// The task marked done.
        id: TaskId,
        /// Why, in the operator's own words.
        reason: String,
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
    /// The fine-grained outcome the agent itself reported for this step, when this is a step's
    /// own ending: `None` for the attempt's own ending, and for a step — the sync and
    /// health-check steps — that the tool records as already passed, which no agent ever
    /// reports an outcome for.
    pub reported: Option<Outcome>,
}

/// One step of an attempt, as [`crate::attempt::last_attempt`] reports it: the pipeline every
/// task's attempt runs through is an ordered list of these, one per name in it, run in order
/// and stopping at the first that ends badly — so a step later in the list has none of these at
/// all when an earlier one stopped the attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The step's name.
    pub name: String,
    /// When it started.
    pub started_at: SystemTime,
    /// How it ended, once [`crate::attempt::end_step`] has recorded it; `None` while it runs.
    pub ended: Option<AttemptEnd>,
}

/// A task's most recent attempt, as [`crate::attempt::last_attempt`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The attempt's number.
    pub number: u32,
    /// When it started.
    pub started_at: SystemTime,
    /// The project's commit `HEAD` right before this attempt began. `None` when it could not
    /// be captured.
    pub start_commit: Option<String>,
    /// The provider it ran with, once [`crate::attempt::begin_attempt_running`] has recorded it.
    pub provider: Option<String>,
    /// How it ended, once [`crate::attempt::end_attempt`] has recorded it; `None` while it runs.
    pub ended: Option<AttemptEnd>,
    /// Every step run so far, in the order they were started.
    pub steps: Vec<Step>,
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

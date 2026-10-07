//! Typed facts the status use case returns for interfaces to present.

use std::time::{Duration, SystemTime};

use crate::{LimitWait, LimitWarning, Routed, TaskId, TaskStatus, Usage, WaitReason};

use super::AttemptOutcome;

/// A live wait: why a step is waiting and how long is left of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// What the step is waiting for.
    pub reason: WaitReason,
    /// How long remains until the step runs again.
    pub remaining: Duration,
}

/// One step of an attempt, as the status use case observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepLine {
    /// The step's journal name.
    pub step: String,
    /// The provider that ran the step, when an agent ran it.
    pub provider: Option<String>,
    /// The model configured for the step, when it has one.
    pub model: Option<String>,
    /// The provider session the implementation step reported, when it did.
    pub session: Option<String>,
    /// Recorded duration, or elapsed duration while the step runs.
    pub time_spent: Duration,
    /// The observed outcome.
    pub outcome: AttemptOutcome,
    /// The recorded reason, when the journal has one.
    pub reason: Option<String>,
    /// What the step is waiting for, and how long remains, while it waits.
    pub waiting: Option<Wait>,
    /// The provider-limit wait recorded after the step resumed.
    pub limit_wait: Option<LimitWait>,
    /// A non-blocking provider-limit warning emitted while the step ran.
    pub limit_warning: Option<LimitWarning>,
    /// Provider token and cost figures, or none for a provider that reported no usage.
    pub usage: Usage,
    /// What the router decided to do next after the step ended badly, when it did.
    pub routed: Option<Routed>,
    /// The time the router's decision added to the step's limit, when it added any.
    pub more_time: Option<Duration>,
}

/// One task attempt, including its current step and every step it has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLine {
    /// The attempt number.
    pub number: u32,
    /// The current step's journal name.
    pub step: String,
    /// The current step's provider, when an agent ran it.
    pub provider: Option<String>,
    /// The current step's model, when it has one.
    pub model: Option<String>,
    /// The current implementation session, when it has one.
    pub session: Option<String>,
    /// The current step's duration.
    pub time_spent: Duration,
    /// The current step's observed outcome.
    pub outcome: AttemptOutcome,
    /// The current step's recorded reason.
    pub reason: Option<String>,
    /// What the current step is waiting for, and how long remains, while it waits.
    pub waiting: Option<Wait>,
    /// The current step's completed provider-limit wait.
    pub limit_wait: Option<LimitWait>,
    /// A non-blocking provider-limit warning emitted while the current step ran.
    pub limit_warning: Option<LimitWarning>,
    /// Live provider-output facts, when they are available.
    pub output_activity: Option<OutputActivity>,
    /// Every started step, in order.
    pub steps: Vec<StepLine>,
    /// Total provider usage across every completed step in this attempt.
    pub usage: Usage,
    /// What the router decided after the attempt's step that ended badly, when it did.
    pub routed: Option<Routed>,
    /// The time the router's decision added to this attempt's limit, when it added any.
    pub more_time: Option<Duration>,
}

/// What an append-only provider output stream says about a running attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputActivity {
    /// When provider output was most recently received, when it has written anything.
    pub last_output_at: Option<SystemTime>,
    /// How long the provider has been silent.
    pub silent_for: Duration,
    /// Whether output is recent enough that an activity indicator should advance.
    pub active: bool,
    /// Whether the configured silence threshold has passed.
    pub may_be_stuck: bool,
}

/// Who marked a task done without an attempt reporting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneMark {
    /// The operator's recorded reason.
    pub reason: String,
    /// When the task was marked done.
    pub at: SystemTime,
}

/// One task with recorded status history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The task identifier.
    pub task: TaskId,
    /// The task title.
    pub title: String,
    /// The task's journal state.
    pub status: TaskStatus,
    /// Its latest attempt.
    pub attempt: AttemptLine,
    /// Earlier attempts, oldest first.
    pub history: Vec<AttemptLine>,
    /// The manual done mark, when the operator made one.
    pub done_by_user: Option<DoneMark>,
}

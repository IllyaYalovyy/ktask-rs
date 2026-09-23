//! `TaskState`, `Phase` and `PauseReason`: the supervisor's lifecycle
//! vocabulary — what state a task is in, where an attempt stands within a
//! protocol, and why a task is currently paused. `apply` is the single
//! function through which a `TaskState` may change, per `VISION.md` §6.
//!
//! The pipeline mirrors `docs/DESIGN.md`'s event catalog ordering, which
//! lists each stage's `*Started` event before its `*Passed`/`*Failed` event
//! before the next stage's `*Started` event. `apply` follows that ordering
//! literally: entering a stage and recording that stage's own success both
//! happen without moving further, and only the *next* stage's `Started`-like
//! event advances custody. So `PreflightPassed` leaves the state at
//! `Preflight` (it has nowhere else to put `base_sha`); `AttemptStarted` is
//! what moves it to `Running`. The same pattern holds for `Verifying`:
//! `VerifyPassed` leaves it at `Verifying`, and `PublishStarted` is what
//! moves it to `Publishing`. `Running`/`Remediating` reach `Verifying` not
//! through a distinct event but by entering the shared `Phase::Verify`,
//! since a work protocol's mandatory completion gates are just its last
//! phase.
//!
//! `AttemptStarted` does not carry a phase (that is `PhaseEntered`'s job), so
//! it seeds `Running` with `Phase::Implement` — correct for `direct`, and
//! immediately corrected for other protocols by the `PhaseEntered` event the
//! runner always emits before doing any protocol-specific work. The same
//! reasoning seeds `Remediating` on `VerifyFailed`.

use crate::Result;
use crate::classify::{FailureClass, Recovery};
use crate::error::Error;
use crate::event::EventKind;
use crate::ids::{AttemptId, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// Where a task stands in the supervisor's custody of it.
///
/// Pipeline states move left to right: `Queued -> Preflight -> Running ->
/// Verifying -> Publishing -> PublishedVerified -> Done`, with `Running`
/// able to loop back through `Remediating` (bounded by
/// `max_remediation_attempts`). `Paused` suspends any of those pipeline
/// states and remembers exactly where to resume. `Done`, `Acknowledged`,
/// `Failed` and `Cancelled` are terminal, per `VISION.md` §6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskState {
    /// Waiting for its turn; no attempt has started.
    Queued,
    /// Proving the world is sane before spending tokens: clean fetched
    /// mainline, green baseline, provider available, disk space, lock held.
    Preflight,
    /// An attempt is executing the named phase of its protocol.
    Running {
        /// Which attempt is running.
        attempt: AttemptId,
        /// The phase it is currently in.
        phase: Phase,
    },
    /// A bounded retry after `Running` failed verification, executing the
    /// named phase of its protocol.
    Remediating {
        /// Which attempt is remediating.
        attempt: AttemptId,
        /// The phase it is currently in.
        phase: Phase,
    },
    /// Running the mandatory completion gates against the attempt's result.
    Verifying {
        /// Which attempt is being verified.
        attempt: AttemptId,
    },
    /// Publishing the verified result to mainline.
    Publishing {
        /// Which attempt is being published.
        attempt: AttemptId,
    },
    /// Published and confirmed present on the remote at `commit`.
    PublishedVerified {
        /// The commit SHA confirmed on mainline.
        commit: String,
    },
    /// Fully complete: a terminal success state for an executable task.
    Done,
    /// A gate's terminal success state: a human confirmed it via `ack`.
    Acknowledged {
        /// Who acknowledged the gate.
        by: String,
        /// When they acknowledged it.
        at: OffsetDateTime,
    },
    /// Suspended, remembering the state to resume into.
    Paused {
        /// Why the task is paused.
        reason: PauseReason,
        /// The state to return to once the pause is resolved.
        resume_to: Box<TaskState>,
    },
    /// Terminally failed; no further attempts will run unless a human
    /// retries or cancels it.
    Failed {
        /// The class of failure.
        class: FailureClass,
        /// A human-readable description of what went wrong.
        detail: String,
    },
    /// Terminally cancelled by a human.
    Cancelled,
}

impl TaskState {
    /// Whether this state is terminal: no further transition occurs without
    /// external intervention (a new task, a requeue, or similar).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Done
                | TaskState::Acknowledged { .. }
                | TaskState::Failed { .. }
                | TaskState::Cancelled
        )
    }

    /// Whether this state is a durable pause: the task is suspended and
    /// remembers where to resume.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self, TaskState::Paused { .. })
    }

    /// Whether an attempt is actively in flight for this task: past
    /// `Queued` (waiting for its turn is not activity), not paused, and not
    /// terminal. `VISION.md` §3 invariant 1 permits at most one task in the
    /// whole queue to report `true` here.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TaskState::Preflight
                | TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
                | TaskState::Publishing { .. }
                | TaskState::PublishedVerified { .. }
        )
    }

    /// The variant's name, stable for logging, display and error messages.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            TaskState::Queued => "Queued",
            TaskState::Preflight => "Preflight",
            TaskState::Running { .. } => "Running",
            TaskState::Remediating { .. } => "Remediating",
            TaskState::Verifying { .. } => "Verifying",
            TaskState::Publishing { .. } => "Publishing",
            TaskState::PublishedVerified { .. } => "PublishedVerified",
            TaskState::Done => "Done",
            TaskState::Acknowledged { .. } => "Acknowledged",
            TaskState::Paused { .. } => "Paused",
            TaskState::Failed { .. } => "Failed",
            TaskState::Cancelled => "Cancelled",
        }
    }
}

/// Enforces `VISION.md` §3 invariant 1: exactly one task is active at a
/// time; parallel execution does not exist in v1.
///
/// `Queued` and `Paused` do not count as active: a queue full of tasks
/// waiting for their turn, or parked pending a human or a limit, is the
/// normal resting state. Only [`TaskState::is_active`] states — an attempt
/// genuinely in flight — are counted.
///
/// # Errors
///
/// Returns [`Error::Policy`] naming every active task's id if more than one
/// task in `states` is active.
pub fn check_one_active(states: &BTreeMap<TaskId, TaskState>) -> Result<()> {
    let active: Vec<TaskId> = states
        .iter()
        .filter(|(_, state)| state.is_active())
        .map(|(id, _)| *id)
        .collect();

    if active.len() > 1 {
        let names = active
            .iter()
            .map(TaskId::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::Policy {
            detail: format!("more than one task is active: {names}"),
            paths: Vec::new(),
        });
    }
    Ok(())
}

/// Enforces `VISION.md` §3 invariant 2: `next` cannot start until every task
/// ahead of it in the queue has reached a settled state — `Done`,
/// `Cancelled`, `PublishedVerified` (already confirmed present on mainline,
/// so a successor may safely build on it without waiting for the bookkeeping
/// `TaskDone` event that follows), or `Acknowledged` (a human gate's own
/// terminal success — it never publishes anything, so `Acknowledged` is as
/// settled as a gate entry gets).
///
/// # Errors
///
/// Returns [`Error::Policy`] naming the lowest-id predecessor of `next` that
/// has not reached one of those states.
pub fn check_predecessor(states: &BTreeMap<TaskId, TaskState>, next: TaskId) -> Result<()> {
    for (id, state) in states.range(..next) {
        let settled = matches!(
            state,
            TaskState::Done
                | TaskState::Cancelled
                | TaskState::PublishedVerified { .. }
                | TaskState::Acknowledged { .. }
        );
        if !settled {
            return Err(Error::Policy {
                detail: format!(
                    "task {next} cannot start: predecessor {id} is not published (state: {})",
                    state.name()
                ),
                paths: Vec::new(),
            });
        }
    }
    Ok(())
}

/// Builds the standard "this event does not apply here" error, naming the
/// state by [`TaskState::name`] and the event by its discriminant.
fn invalid(from: &str, event: &EventKind) -> Error {
    Error::InvalidTransition {
        from: from.to_string(),
        event: event.discriminant().to_string(),
    }
}

/// Applies `event` to `state`, returning the state that results.
///
/// The only way a [`TaskState`] changes. Pure: no I/O, no clock, no
/// randomness. Terminal states (`Done`, `Acknowledged`, `Cancelled`) accept
/// no event at all, and `Failed` accepts only the two external interventions
/// a failure allows: [`EventKind::RetryStarted`] (`ktask-rs retry`) and
/// [`EventKind::TaskCancelled`] (`ktask-rs cancel`, which lets the queue
/// proceed past it); every other state delegates to one helper below, which
/// matches `event` exhaustively.
///
/// # Errors
///
/// Returns [`Error::InvalidTransition`] if `event` does not apply to
/// `state`, including every event given to a terminal state.
pub fn apply(state: &TaskState, event: &EventKind) -> Result<TaskState> {
    match state {
        TaskState::Queued => from_queued(event),
        TaskState::Preflight => from_preflight(event),
        TaskState::Running { attempt, phase } => from_running(*attempt, *phase, event),
        TaskState::Remediating { attempt, phase } => from_remediating(*attempt, *phase, event),
        TaskState::Verifying { attempt } => from_verifying(*attempt, event),
        TaskState::Publishing { attempt } => from_publishing(*attempt, event),
        TaskState::PublishedVerified { commit } => from_published_verified(commit, event),
        TaskState::Paused { reason, resume_to } => from_paused(reason, resume_to, event),
        TaskState::Failed { .. } => match event {
            EventKind::RetryStarted { attempt } => Ok(TaskState::Remediating {
                attempt: *attempt,
                phase: Phase::Implement,
            }),
            EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
            _ => Err(invalid(state.name(), event)),
        },
        TaskState::Done | TaskState::Acknowledged { .. } | TaskState::Cancelled => {
            Err(invalid(state.name(), event))
        }
    }
}

/// Transitions from [`TaskState::Queued`]: waiting for its turn.
fn from_queued(event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::TaskQueued { .. } => Ok(TaskState::Queued),
        EventKind::PreflightStarted => Ok(TaskState::Preflight),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Queued),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => Err(invalid("Queued", event)),
    }
}

/// Transitions from [`TaskState::Preflight`]: proving the world is sane.
fn from_preflight(event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::PreflightPassed { .. } => Ok(TaskState::Preflight),
        EventKind::PreflightFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::AttemptStarted { attempt, .. } => Ok(TaskState::Running {
            attempt: *attempt,
            phase: Phase::Implement,
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Preflight),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Preflight),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => Err(invalid("Preflight", event)),
    }
}

/// Transitions from [`TaskState::Running`]: an attempt executing `phase`.
///
/// Entering `Phase::Verify` is how `Running` reaches [`TaskState::Verifying`]:
/// the mandatory completion gates are just a work protocol's last phase, not
/// a separately-triggered event.
fn from_running(attempt: AttemptId, phase: Phase, event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::PhaseEntered {
            phase: Phase::Verify,
            ..
        } => Ok(TaskState::Verifying { attempt }),
        EventKind::PhaseEntered { phase, .. } => Ok(TaskState::Running {
            attempt,
            phase: *phase,
        }),
        EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::TddExceptionUsed { .. } => Ok(TaskState::Running { attempt, phase }),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::DecisionRaised { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Running { attempt, phase }),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Running { attempt, phase }),
        }),
        EventKind::Interrupted { phase } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Running {
                attempt,
                phase: *phase,
            }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => Err(invalid("Running", event)),
    }
}

/// Transitions from [`TaskState::Remediating`]: a bounded retry executing
/// `phase`, entered only from [`TaskState::Verifying`] on `VerifyFailed`.
///
/// `SelfHealingReport` is accepted here, and only here, as a no-op alongside
/// `AttemptRecorded`: `VISION.md` §7's "every recovery produces a
/// self-healing report" describes a remediation attempt, so the report is
/// evidence for the attempt still in flight in this state, recorded without
/// moving custody, just as `AttemptRecorded` is.
///
/// Mirrors [`from_running`]: whether another remediation attempt is even
/// allowed is a bound the runner checks before emitting the next event, not
/// something this pure function can know.
fn from_remediating(attempt: AttemptId, phase: Phase, event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::PhaseEntered {
            phase: Phase::Verify,
            ..
        } => Ok(TaskState::Verifying { attempt }),
        EventKind::PhaseEntered { phase, .. } => Ok(TaskState::Remediating {
            attempt,
            phase: *phase,
        }),
        EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::SelfHealingReport { .. } => Ok(TaskState::Remediating { attempt, phase }),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::DecisionRaised { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Remediating { attempt, phase }),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Remediating { attempt, phase }),
        }),
        EventKind::Interrupted { phase } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Remediating {
                attempt,
                phase: *phase,
            }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. } => Err(invalid("Remediating", event)),
    }
}

/// Transitions from [`TaskState::Verifying`]: running the mandatory
/// completion gates against `attempt`'s result.
fn from_verifying(attempt: AttemptId, event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::VerifyPassed { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. } => Ok(TaskState::Verifying { attempt }),
        EventKind::VerifyFailed { .. } => Ok(TaskState::Remediating {
            attempt,
            phase: Phase::Implement,
        }),
        EventKind::PublishStarted { .. } => Ok(TaskState::Publishing { attempt }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Verifying { attempt }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Verifying { attempt }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => Err(invalid("Verifying", event)),
    }
}

/// Transitions from [`TaskState::Publishing`]: publishing `attempt`'s
/// verified result to mainline.
///
/// No `TaskCancelled` arm: once publication has started, a commit-and-push
/// against mainline may already be underway, and cancelling mid-flight would
/// leave custody of a live git operation nowhere. `docs/CONTRACT.md`'s
/// `cancel` is for a task the queue can safely skip past, which this is not.
fn from_publishing(attempt: AttemptId, event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::PublishVerified { commit, .. } => Ok(TaskState::PublishedVerified {
            commit: commit.clone(),
        }),
        EventKind::AttemptRecorded { .. } => Ok(TaskState::Publishing { attempt }),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Publishing { attempt }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Publishing { attempt }),
        }),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskCancelled { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => Err(invalid("Publishing", event)),
    }
}

/// Transitions from [`TaskState::PublishedVerified`]: published and
/// confirmed present on the remote at `commit`.
///
/// `VISION.md` §3 invariants 4 and 7: a task is never done on say-so, and
/// completion requires the recorded, verified publication. `TaskDone` is
/// therefore only reachable from here, and only when its own `commit`
/// payload matches the one this state already carries — the one `apply`
/// itself set from `PublishVerified`, not whatever the caller now claims.
fn from_published_verified(commit: &str, event: &EventKind) -> Result<TaskState> {
    match event {
        EventKind::TaskDone { commit: done } if done == commit => Ok(TaskState::Done),
        EventKind::TaskDone { .. }
        | EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::TaskCancelled { .. }
        | EventKind::Paused { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::DecisionResolved { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => {
            Err(invalid(&format!("PublishedVerified({commit})"), event))
        }
    }
}

/// Transitions from [`TaskState::Paused`]: suspended for `reason`,
/// remembering `resume_to`.
///
/// Which event ends a pause depends on why it began. A `HumanGate` pause
/// leaves through `GateAcknowledged` alone (matching `docs/CONTRACT.md`'s
/// `ack`, and `VISION.md` §6's "a gate ... reaches `acknowledged` through
/// `ktask-rs ack` rather than through publication"). An `Input` pause leaves
/// through `Resumed` (back to `resume_to`) or `DecisionResolved`, which sends
/// the task to `Queued` instead: the answer changes its context, so it starts
/// over with a fresh attempt (`docs/adr/0009-*.md`). Every other pause reason
/// leaves through `Resumed` alone. `RecoveryDecision`
/// only applies to an `Interrupted` pause: `Resume` and `AlreadyApplied` both
/// hand custody back to `resume_to` (nothing left to redo in either case, per
/// `VISION.md` §6), while `MarkInterrupted` leaves the task parked exactly
/// where it was, now with that decision on the record.
fn from_paused(
    reason: &PauseReason,
    resume_to: &TaskState,
    event: &EventKind,
) -> Result<TaskState> {
    match event {
        EventKind::Resumed => match reason {
            PauseReason::HumanGate => Err(invalid("Paused", event)),
            PauseReason::Limit { .. }
            | PauseReason::Input
            | PauseReason::Interrupted
            | PauseReason::Blocked => Ok(resume_to.clone()),
        },
        EventKind::GateAcknowledged { by, at } => match reason {
            PauseReason::HumanGate => Ok(TaskState::Acknowledged {
                by: by.clone(),
                at: *at,
            }),
            PauseReason::Limit { .. }
            | PauseReason::Input
            | PauseReason::Interrupted
            | PauseReason::Blocked => Err(invalid("Paused", event)),
        },
        EventKind::DecisionResolved { .. } => match reason {
            PauseReason::Input => Ok(TaskState::Queued),
            PauseReason::Limit { .. }
            | PauseReason::HumanGate
            | PauseReason::Interrupted
            | PauseReason::Blocked => Err(invalid("Paused", event)),
        },
        EventKind::RecoveryDecision { decision, .. } => match reason {
            PauseReason::Interrupted => match decision {
                Recovery::Resume | Recovery::AlreadyApplied => Ok(resume_to.clone()),
                Recovery::MarkInterrupted => Ok(TaskState::Paused {
                    reason: reason.clone(),
                    resume_to: Box::new(resume_to.clone()),
                }),
            },
            PauseReason::Limit { .. }
            | PauseReason::Input
            | PauseReason::HumanGate
            | PauseReason::Blocked => Err(invalid("Paused", event)),
        },
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Paused { .. }
        | EventKind::Interrupted { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => Err(invalid("Paused", event)),
    }
}

/// A step within a protocol's execution of an attempt.
///
/// Carries every phase any protocol needs — including `spec-first`'s
/// `Goal`, `Scope`, `AcceptanceTests`, `Review`, `Harden` and `DoneCheck` —
/// so no later task has to widen this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// State the outcome the task must achieve.
    Goal,
    /// State what is, and is not, in scope.
    Scope,
    /// Write the tests that prove the outcome was achieved.
    AcceptanceTests,
    /// Implement, for protocols that do not separate red/green/refactor.
    Implement,
    /// Write a failing test; production code paths are read-only.
    Red,
    /// Make the failing test pass with the smallest change that does so.
    Green,
    /// Clean up while the tests from `Red`/`Green` stay green.
    Refactor,
    /// Review the change before hardening it.
    Review,
    /// Address edge cases and robustness.
    Harden,
    /// Check the task's `Done-when` criteria are satisfied.
    DoneCheck,
    /// Run the mandatory completion gates.
    Verify,
    /// Publish the verified result.
    Publish,
}

/// Why a task is currently paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// A provider usage limit was hit; resumes after `until`, when known.
    Limit {
        /// When the limit is expected to lift, if the provider reported one.
        until: Option<OffsetDateTime>,
    },
    /// The task is blocked on information only a human can supply.
    Input,
    /// The task is blocked on a human's explicit approval to proceed.
    HumanGate,
    /// The run was interrupted, for example by a process kill or a restart.
    Interrupted,
    /// The task is blocked on another task or an external condition.
    Blocked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Stream, TddException};
    use crate::gate::{GateKind, GateResult};

    fn sample_gate_result() -> GateResult {
        GateResult {
            kind: GateKind::Targeted,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 5,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    fn all_phases() -> Vec<Phase> {
        vec![
            Phase::Goal,
            Phase::Scope,
            Phase::AcceptanceTests,
            Phase::Implement,
            Phase::Red,
            Phase::Green,
            Phase::Refactor,
            Phase::Review,
            Phase::Harden,
            Phase::DoneCheck,
            Phase::Verify,
            Phase::Publish,
        ]
    }

    #[test]
    fn phase_has_exactly_twelve_variants() {
        let variants = all_phases();
        assert_eq!(variants.len(), 12);

        // Exhaustive, wildcard-free match: if a variant is ever added to
        // `Phase` without being listed here too, this stops compiling
        // instead of silently under-counting.
        for phase in variants {
            match phase {
                Phase::Goal
                | Phase::Scope
                | Phase::AcceptanceTests
                | Phase::Implement
                | Phase::Red
                | Phase::Green
                | Phase::Refactor
                | Phase::Review
                | Phase::Harden
                | Phase::DoneCheck
                | Phase::Verify
                | Phase::Publish => {}
            }
        }
    }

    #[test]
    fn every_phase_round_trips_through_json() {
        for phase in all_phases() {
            let json = serde_json::to_string(&phase).expect("serialize");
            let back: Phase = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(phase, back);
        }
    }

    fn all_pause_reasons() -> Vec<PauseReason> {
        vec![
            PauseReason::Limit { until: None },
            PauseReason::Input,
            PauseReason::HumanGate,
            PauseReason::Interrupted,
            PauseReason::Blocked,
        ]
    }

    #[test]
    fn pause_reason_has_exactly_five_variants() {
        let variants = all_pause_reasons();
        assert_eq!(variants.len(), 5);

        for reason in variants {
            match reason {
                PauseReason::Limit { until: _ }
                | PauseReason::Input
                | PauseReason::HumanGate
                | PauseReason::Interrupted
                | PauseReason::Blocked => {}
            }
        }
    }

    #[test]
    fn every_pause_reason_round_trips_through_json() {
        for reason in all_pause_reasons() {
            let json = serde_json::to_string(&reason).expect("serialize");
            let back: PauseReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(reason, back);
        }
    }

    #[test]
    fn pause_reason_limit_with_a_known_time_round_trips_through_json() {
        let reason = PauseReason::Limit {
            until: Some(OffsetDateTime::UNIX_EPOCH),
        };
        let json = serde_json::to_string(&reason).expect("serialize");
        let back: PauseReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(reason, back);
    }

    fn all_task_states() -> Vec<TaskState> {
        vec![
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Harden,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            TaskState::PublishedVerified {
                commit: "abc123".to_string(),
            },
            TaskState::Done,
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Green,
                }),
            },
            TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: "tests failed".to_string(),
            },
            TaskState::Cancelled,
        ]
    }

    #[test]
    fn task_state_has_exactly_twelve_variants() {
        let variants = all_task_states();
        assert_eq!(variants.len(), 12);

        // Exhaustive, wildcard-free match: if a variant is ever added to
        // `TaskState` without being listed here too, this stops compiling
        // instead of silently under-counting.
        for state in variants {
            match state {
                TaskState::Queued
                | TaskState::Preflight
                | TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
                | TaskState::Publishing { .. }
                | TaskState::PublishedVerified { .. }
                | TaskState::Done
                | TaskState::Acknowledged { .. }
                | TaskState::Paused { .. }
                | TaskState::Failed { .. }
                | TaskState::Cancelled => {}
            }
        }
    }

    #[test]
    fn every_task_state_round_trips_through_json() {
        for state in all_task_states() {
            let json = serde_json::to_string(&state).expect("serialize");
            let back: TaskState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(state, back);
        }
    }

    #[test]
    fn paused_state_round_trips_its_boxed_resume_state_through_json() {
        let state = TaskState::Paused {
            reason: PauseReason::Limit {
                until: Some(OffsetDateTime::UNIX_EPOCH),
            },
            resume_to: Box::new(TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Verifying {
                    attempt: AttemptId::new(3),
                }),
            }),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, back);

        match back {
            TaskState::Paused { resume_to, .. } => match *resume_to {
                TaskState::Paused { resume_to, .. } => {
                    assert_eq!(
                        *resume_to,
                        TaskState::Verifying {
                            attempt: AttemptId::new(3)
                        }
                    );
                }
                other => panic!("expected nested Paused, got {other:?}"),
            },
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[test]
    fn terminal_states_report_is_terminal_true() {
        assert!(TaskState::Done.is_terminal());
        assert!(
            TaskState::Acknowledged {
                by: "bob".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            }
            .is_terminal()
        );
        assert!(
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "crashed".to_string(),
            }
            .is_terminal()
        );
        assert!(TaskState::Cancelled.is_terminal());
    }

    #[test]
    fn non_terminal_states_report_is_terminal_false() {
        for state in all_task_states() {
            let expect_terminal = matches!(
                state,
                TaskState::Done
                    | TaskState::Acknowledged { .. }
                    | TaskState::Failed { .. }
                    | TaskState::Cancelled
            );
            assert_eq!(state.is_terminal(), expect_terminal, "state: {state:?}");
        }
    }

    #[test]
    fn only_paused_reports_is_paused_true() {
        for state in all_task_states() {
            let expect_paused = matches!(state, TaskState::Paused { .. });
            assert_eq!(state.is_paused(), expect_paused, "state: {state:?}");
        }
    }

    #[test]
    fn name_returns_the_variant_name_for_every_state() {
        assert_eq!(TaskState::Queued.name(), "Queued");
        assert_eq!(TaskState::Preflight.name(), "Preflight");
        assert_eq!(
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }
            .name(),
            "Running"
        );
        assert_eq!(
            TaskState::Remediating {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }
            .name(),
            "Remediating"
        );
        assert_eq!(
            TaskState::Verifying {
                attempt: AttemptId::new(1)
            }
            .name(),
            "Verifying"
        );
        assert_eq!(
            TaskState::Publishing {
                attempt: AttemptId::new(1)
            }
            .name(),
            "Publishing"
        );
        assert_eq!(
            TaskState::PublishedVerified {
                commit: "sha".to_string()
            }
            .name(),
            "PublishedVerified"
        );
        assert_eq!(TaskState::Done.name(), "Done");
        assert_eq!(
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            }
            .name(),
            "Acknowledged"
        );
        assert_eq!(
            TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Queued),
            }
            .name(),
            "Paused"
        );
        assert_eq!(
            TaskState::Failed {
                class: FailureClass::PolicyFailure,
                detail: "nope".to_string(),
            }
            .name(),
            "Failed"
        );
        assert_eq!(TaskState::Cancelled.name(), "Cancelled");
    }

    fn attempt_recorded(attempt: AttemptId) -> EventKind {
        EventKind::AttemptRecorded {
            record: Box::new(crate::AttemptRecord {
                id: attempt,
                task: TaskId::new(1),
                started: OffsetDateTime::UNIX_EPOCH,
                ended: None,
                model_configured: None,
                model_reported: None,
                session_id: None,
                exit_reason: "completed".to_string(),
                gates: Vec::new(),
                usage: None,
                base_sha: "base".to_string(),
                candidate_sha: None,
            }),
        }
    }

    fn attempt_started(attempt: AttemptId) -> EventKind {
        EventKind::AttemptStarted {
            attempt,
            protocol: "direct".to_string(),
            pid: 4242,
            base_sha: "base".to_string(),
        }
    }

    fn self_healing_report(attempt: AttemptId) -> EventKind {
        EventKind::SelfHealingReport {
            attempt,
            class: FailureClass::VerificationFailure,
            repairs: vec!["reran the failing test after a targeted fix".to_string()],
            outcome: "verification passed on retry".to_string(),
        }
    }

    fn decision_raised() -> EventKind {
        EventKind::DecisionRaised {
            request: crate::DecisionRequest {
                question: "Postgres or SQLite?".to_string(),
                options: vec!["Postgres".to_string(), "SQLite".to_string()],
                tradeoffs: "t".to_string(),
                impact: "i".to_string(),
                recommended: None,
            },
        }
    }

    fn decision_resolved() -> EventKind {
        EventKind::DecisionResolved {
            adr_path: std::path::PathBuf::from("docs/adr/0009-storage.md"),
            answer: "SQLite".to_string(),
        }
    }

    #[test]
    fn the_direct_protocol_happy_path_reaches_done_event_by_event() {
        let attempt = AttemptId::new(1);

        let state = TaskState::Queued;
        let state = apply(
            &state,
            &EventKind::TaskQueued {
                title: "Add widget".to_string(),
            },
        )
        .expect("TaskQueued");
        assert_eq!(state, TaskState::Queued);

        let state = apply(&state, &EventKind::PreflightStarted).expect("PreflightStarted");
        assert_eq!(state, TaskState::Preflight);

        let state = apply(
            &state,
            &EventKind::PreflightPassed {
                base_sha: "abc123".to_string(),
            },
        )
        .expect("PreflightPassed");
        assert_eq!(state, TaskState::Preflight);

        let state = apply(&state, &attempt_started(attempt)).expect("AttemptStarted");
        assert_eq!(
            state,
            TaskState::Running {
                attempt,
                phase: Phase::Implement,
            }
        );

        let state = apply(
            &state,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )
        .expect("PhaseEntered(Verify)");
        assert_eq!(state, TaskState::Verifying { attempt });

        let state = apply(&state, &EventKind::VerifyPassed { attempt }).expect("VerifyPassed");
        assert_eq!(state, TaskState::Verifying { attempt });

        let state = apply(
            &state,
            &EventKind::PublishStarted {
                attempt,
                candidate_sha: "def456".to_string(),
            },
        )
        .expect("PublishStarted");
        assert_eq!(state, TaskState::Publishing { attempt });

        let state = apply(
            &state,
            &EventKind::PublishVerified {
                commit: "def456".to_string(),
                remote_sha: "def456".to_string(),
            },
        )
        .expect("PublishVerified");
        assert_eq!(
            state,
            TaskState::PublishedVerified {
                commit: "def456".to_string(),
            }
        );

        let state = apply(
            &state,
            &EventKind::TaskDone {
                commit: "def456".to_string(),
            },
        )
        .expect("TaskDone");
        assert_eq!(state, TaskState::Done);
    }

    #[test]
    fn a_verification_failure_loops_through_remediating_back_to_verifying() {
        let attempt = AttemptId::new(1);
        let state = TaskState::Verifying { attempt };

        let state = apply(
            &state,
            &EventKind::VerifyFailed {
                attempt,
                class: FailureClass::VerificationFailure,
                detail: "clippy failed".to_string(),
            },
        )
        .expect("VerifyFailed");
        assert_eq!(
            state,
            TaskState::Remediating {
                attempt,
                phase: Phase::Implement,
            }
        );

        let state = apply(
            &state,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Refactor,
            },
        )
        .expect("PhaseEntered(Refactor)");
        assert_eq!(
            state,
            TaskState::Remediating {
                attempt,
                phase: Phase::Refactor,
            }
        );

        let state = apply(
            &state,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )
        .expect("PhaseEntered(Verify)");
        assert_eq!(state, TaskState::Verifying { attempt });
    }

    #[test]
    fn from_queued_accepts_task_queued_and_stays_queued() {
        let state = apply(
            &TaskState::Queued,
            &EventKind::TaskQueued {
                title: "t".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Queued);
    }

    #[test]
    fn from_queued_accepts_preflight_started() {
        let state = apply(&TaskState::Queued, &EventKind::PreflightStarted).expect("legal");
        assert_eq!(state, TaskState::Preflight);
    }

    #[test]
    fn from_queued_accepts_pause_and_remembers_queued_as_resume_to() {
        let state = apply(
            &TaskState::Queued,
            &EventKind::Paused {
                reason: PauseReason::Blocked,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Queued),
            }
        );
    }

    #[test]
    fn from_queued_accepts_task_cancelled() {
        let state = apply(
            &TaskState::Queued,
            &EventKind::TaskCancelled {
                reason: "superseded".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn from_queued_rejects_gate_started() {
        let err = apply(
            &TaskState::Queued,
            &EventKind::GateStarted {
                gate: GateKind::Targeted,
            },
        )
        .expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Queued");
                assert_eq!(event, "GateStarted");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_queued_rejects_verify_passed() {
        let attempt = AttemptId::new(1);
        let err =
            apply(&TaskState::Queued, &EventKind::VerifyPassed { attempt }).expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Queued");
                assert_eq!(event, "VerifyPassed");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_preflight_accepts_preflight_passed_and_stays_preflight() {
        let state = apply(
            &TaskState::Preflight,
            &EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Preflight);
    }

    #[test]
    fn from_preflight_accepts_preflight_failed_and_fails_terminally() {
        let state = apply(
            &TaskState::Preflight,
            &EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "disk full".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Failed {
                class: FailureClass::EnvironmentFailure,
                detail: "disk full".to_string(),
            }
        );
    }

    #[test]
    fn from_preflight_accepts_attempt_started_and_seeds_the_implement_phase() {
        let attempt = AttemptId::new(1);
        let state = apply(&TaskState::Preflight, &attempt_started(attempt)).expect("legal");
        assert_eq!(
            state,
            TaskState::Running {
                attempt,
                phase: Phase::Implement,
            }
        );
    }

    #[test]
    fn from_preflight_accepts_interrupted_and_pauses_remembering_preflight() {
        let state = apply(
            &TaskState::Preflight,
            &EventKind::Interrupted { phase: Phase::Goal },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Preflight),
            }
        );
    }

    #[test]
    fn from_preflight_accepts_task_cancelled() {
        let state = apply(
            &TaskState::Preflight,
            &EventKind::TaskCancelled {
                reason: "superseded".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn from_preflight_rejects_phase_entered() {
        let attempt = AttemptId::new(1);
        let err = apply(
            &TaskState::Preflight,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Implement,
            },
        )
        .expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Preflight");
                assert_eq!(event, "PhaseEntered");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_running_accepts_phase_entered_and_moves_phase() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Red,
        };
        let state = apply(
            &running,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Green,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Running {
                attempt,
                phase: Phase::Green,
            }
        );
    }

    #[test]
    fn from_running_entering_verify_phase_reaches_verifying() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Refactor,
        };
        let state = apply(
            &running,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Verifying { attempt });
    }

    #[test]
    fn from_running_accepts_agent_output_without_changing_phase() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Green,
        };
        let state = apply(
            &running,
            &EventKind::AgentOutput {
                attempt,
                stream: Stream::Stdout,
                text: "running tests".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, running);
    }

    #[test]
    fn from_running_accepts_gate_started_and_gate_finished_without_changing_phase() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Red,
        };

        let started = apply(
            &running,
            &EventKind::GateStarted {
                gate: GateKind::Targeted,
            },
        )
        .expect("legal");
        assert_eq!(started, running);

        let finished = apply(
            &running,
            &EventKind::GateFinished {
                result: sample_gate_result(),
            },
        )
        .expect("legal");
        assert_eq!(finished, running);
    }

    #[test]
    fn from_running_accepts_attempt_recorded_without_changing_phase() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Green,
        };
        let state = apply(&running, &attempt_recorded(attempt)).expect("legal");
        assert_eq!(state, running);
    }

    #[test]
    fn from_running_accepts_task_failed() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(
            &running,
            &EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "agent crashed".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "agent crashed".to_string(),
            }
        );
    }

    #[test]
    fn from_running_accepts_decision_raised_and_pauses_for_input() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Green,
        };
        let state = apply(&running, &decision_raised()).expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(running),
            }
        );
    }

    #[test]
    fn from_running_accepts_interrupted_and_pauses_at_the_events_phase() {
        let attempt = AttemptId::new(1);
        // The stored phase is the placeholder `Implement` seeded by
        // `AttemptStarted`; the interruption is authoritative about the real
        // phase in flight, which `resume_to` must reflect.
        let running = TaskState::Running {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(&running, &EventKind::Interrupted { phase: Phase::Red }).expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt,
                    phase: Phase::Red,
                }),
            }
        );
    }

    #[test]
    fn from_running_accepts_task_cancelled() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(
            &running,
            &EventKind::TaskCancelled {
                reason: "superseded".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn from_running_rejects_attempt_started() {
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Implement,
        };
        let err = apply(&running, &attempt_started(attempt)).expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Running");
                assert_eq!(event, "AttemptStarted");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_task_leaves_failed_only_through_retry_started_into_remediating_that_attempt() {
        let failed = TaskState::Failed {
            class: FailureClass::VerificationFailure,
            detail: "gate red".to_string(),
        };
        let attempt = AttemptId::new(3);

        let state = apply(&failed, &EventKind::RetryStarted { attempt }).expect("legal");

        assert_eq!(
            state,
            TaskState::Remediating {
                attempt,
                phase: Phase::Implement,
            }
        );
        // The retry then runs like any remediation round: implement, verify.
        let verifying = apply(
            &state,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )
        .expect("legal");
        assert_eq!(verifying, TaskState::Verifying { attempt });
    }

    #[test]
    fn a_failed_task_can_be_cancelled_so_the_queue_may_proceed_past_it() {
        let failed = TaskState::Failed {
            class: FailureClass::VerificationFailure,
            detail: "gate red".to_string(),
        };

        let state = apply(
            &failed,
            &EventKind::TaskCancelled {
                reason: "not worth fixing".to_string(),
            },
        )
        .expect("legal");

        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn a_resolved_decision_sends_an_input_pause_back_to_queued_not_to_its_resume_point() {
        let paused = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }),
        };

        let state = apply(&paused, &decision_resolved()).expect("legal");

        assert_eq!(state, TaskState::Queued);
    }

    #[test]
    fn a_resolved_decision_is_rejected_by_every_state_that_is_not_an_input_pause() {
        for (label, state) in representative_states() {
            if label == "Paused/Input" {
                continue;
            }
            let result = apply(&state, &decision_resolved());
            assert!(
                matches!(result, Err(Error::InvalidTransition { .. })),
                "{label} must reject DecisionResolved, got {result:?}"
            );
        }
    }

    #[test]
    fn retry_started_is_rejected_by_every_state_that_is_not_failed() {
        for (label, state) in representative_states() {
            if label == "Failed" {
                continue;
            }
            let result = apply(
                &state,
                &EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            );
            assert!(
                matches!(result, Err(Error::InvalidTransition { .. })),
                "{label} must reject RetryStarted, got {result:?}"
            );
        }
    }

    #[test]
    fn from_remediating_accepts_phase_entered_and_moves_phase() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(
            &remediating,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Harden,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Remediating {
                attempt,
                phase: Phase::Harden,
            }
        );
    }

    #[test]
    fn from_remediating_entering_verify_phase_reaches_verifying() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Harden,
        };
        let state = apply(
            &remediating,
            &EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Verifying { attempt });
    }

    #[test]
    fn from_remediating_accepts_agent_output_without_changing_phase() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Harden,
        };
        let state = apply(
            &remediating,
            &EventKind::AgentOutput {
                attempt,
                stream: Stream::Stderr,
                text: "warning".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, remediating);
    }

    #[test]
    fn from_remediating_accepts_gate_started_and_gate_finished_without_changing_phase() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Green,
        };

        let started = apply(
            &remediating,
            &EventKind::GateStarted {
                gate: GateKind::Targeted,
            },
        )
        .expect("legal");
        assert_eq!(started, remediating);

        let finished = apply(
            &remediating,
            &EventKind::GateFinished {
                result: sample_gate_result(),
            },
        )
        .expect("legal");
        assert_eq!(finished, remediating);
    }

    #[test]
    fn from_remediating_accepts_attempt_recorded_without_changing_phase() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Harden,
        };
        let state = apply(&remediating, &attempt_recorded(attempt)).expect("legal");
        assert_eq!(state, remediating);
    }

    #[test]
    fn from_remediating_accepts_self_healing_report_without_changing_phase() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Harden,
        };
        let state = apply(&remediating, &self_healing_report(attempt)).expect("legal");
        assert_eq!(state, remediating);
    }

    #[test]
    fn from_running_rejects_self_healing_report() {
        // A self-healing report describes a remediation attempt, per
        // VISION.md §7; an ordinary (non-remediating) run never produces
        // one, so `Running` must reject it just as it rejects any other
        // event that only applies to `Remediating`.
        let attempt = AttemptId::new(1);
        let running = TaskState::Running {
            attempt,
            phase: Phase::Implement,
        };
        let err = apply(&running, &self_healing_report(attempt)).expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Running");
                assert_eq!(event, "SelfHealingReport");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_remediating_accepts_decision_raised_and_pauses_for_input() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Harden,
        };
        let state = apply(&remediating, &decision_raised()).expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(remediating),
            }
        );
    }

    #[test]
    fn from_remediating_accepts_task_failed_when_the_bound_is_exhausted() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(
            &remediating,
            &EventKind::TaskFailed {
                class: FailureClass::VerificationFailure,
                detail: "remediation attempts exhausted".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: "remediation attempts exhausted".to_string(),
            }
        );
    }

    #[test]
    fn from_remediating_accepts_interrupted_and_pauses_at_the_events_phase() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(
            &remediating,
            &EventKind::Interrupted {
                phase: Phase::Harden,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Remediating {
                    attempt,
                    phase: Phase::Harden,
                }),
            }
        );
    }

    #[test]
    fn from_remediating_accepts_task_cancelled() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Implement,
        };
        let state = apply(
            &remediating,
            &EventKind::TaskCancelled {
                reason: "superseded".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn from_remediating_rejects_publish_started() {
        let attempt = AttemptId::new(2);
        let remediating = TaskState::Remediating {
            attempt,
            phase: Phase::Implement,
        };
        let err = apply(
            &remediating,
            &EventKind::PublishStarted {
                attempt,
                candidate_sha: "sha".to_string(),
            },
        )
        .expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Remediating");
                assert_eq!(event, "PublishStarted");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_verifying_accepts_verify_passed_and_stays_verifying() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Verifying { attempt },
            &EventKind::VerifyPassed { attempt },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Verifying { attempt });
    }

    #[test]
    fn from_verifying_accepts_verify_failed_and_enters_remediation() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Verifying { attempt },
            &EventKind::VerifyFailed {
                attempt,
                class: FailureClass::VerificationFailure,
                detail: "tests failed".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Remediating {
                attempt,
                phase: Phase::Implement,
            }
        );
    }

    #[test]
    fn from_verifying_accepts_publish_started() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Verifying { attempt },
            &EventKind::PublishStarted {
                attempt,
                candidate_sha: "sha".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Publishing { attempt });
    }

    #[test]
    fn from_verifying_accepts_attempt_recorded_and_stays_verifying() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Verifying { attempt },
            &attempt_recorded(attempt),
        )
        .expect("legal");
        assert_eq!(state, TaskState::Verifying { attempt });
    }

    #[test]
    fn from_verifying_accepts_gate_started_and_gate_finished_and_stays_verifying() {
        let attempt = AttemptId::new(1);

        let started = apply(
            &TaskState::Verifying { attempt },
            &EventKind::GateStarted {
                gate: GateKind::Verify,
            },
        )
        .expect("legal");
        assert_eq!(started, TaskState::Verifying { attempt });

        let finished = apply(
            &TaskState::Verifying { attempt },
            &EventKind::GateFinished {
                result: sample_gate_result(),
            },
        )
        .expect("legal");
        assert_eq!(finished, TaskState::Verifying { attempt });
    }

    #[test]
    fn from_verifying_accepts_interrupted_ignoring_its_phase_field() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Verifying { attempt },
            &EventKind::Interrupted {
                phase: Phase::Verify,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Verifying { attempt }),
            }
        );
    }

    #[test]
    fn from_verifying_accepts_task_cancelled() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Verifying { attempt },
            &EventKind::TaskCancelled {
                reason: "superseded".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn from_verifying_rejects_task_done() {
        let attempt = AttemptId::new(1);
        let err = apply(
            &TaskState::Verifying { attempt },
            &EventKind::TaskDone {
                commit: "sha".to_string(),
            },
        )
        .expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Verifying");
                assert_eq!(event, "TaskDone");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_publishing_accepts_publish_verified() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Publishing { attempt },
            &EventKind::PublishVerified {
                commit: "def456".to_string(),
                remote_sha: "def456".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::PublishedVerified {
                commit: "def456".to_string(),
            }
        );
    }

    #[test]
    fn from_publishing_accepts_attempt_recorded_and_stays_publishing() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Publishing { attempt },
            &attempt_recorded(attempt),
        )
        .expect("legal");
        assert_eq!(state, TaskState::Publishing { attempt });
    }

    #[test]
    fn from_publishing_accepts_task_failed_on_git_conflict() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Publishing { attempt },
            &EventKind::TaskFailed {
                class: FailureClass::GitConflict,
                detail: "non-fast-forward".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Failed {
                class: FailureClass::GitConflict,
                detail: "non-fast-forward".to_string(),
            }
        );
    }

    #[test]
    fn from_publishing_accepts_interrupted() {
        let attempt = AttemptId::new(1);
        let state = apply(
            &TaskState::Publishing { attempt },
            &EventKind::Interrupted {
                phase: Phase::Publish,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Publishing { attempt }),
            }
        );
    }

    #[test]
    fn from_publishing_rejects_task_cancelled_mid_flight() {
        let attempt = AttemptId::new(1);
        let err = apply(
            &TaskState::Publishing { attempt },
            &EventKind::TaskCancelled {
                reason: "changed my mind".to_string(),
            },
        )
        .expect_err("illegal: publication may already be underway");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Publishing");
                assert_eq!(event, "TaskCancelled");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_published_verified_accepts_task_done() {
        let state = apply(
            &TaskState::PublishedVerified {
                commit: "def456".to_string(),
            },
            &EventKind::TaskDone {
                commit: "def456".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Done);
    }

    #[test]
    fn from_published_verified_rejects_task_done_with_a_mismatched_commit() {
        let err = apply(
            &TaskState::PublishedVerified {
                commit: "def456".to_string(),
            },
            &EventKind::TaskDone {
                commit: "wrongcommit".to_string(),
            },
        )
        .expect_err("illegal: TaskDone's commit must match the published commit");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "PublishedVerified(def456)");
                assert_eq!(event, "TaskDone");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_published_verified_rejects_task_cancelled() {
        let err = apply(
            &TaskState::PublishedVerified {
                commit: "def456".to_string(),
            },
            &EventKind::TaskCancelled {
                reason: "too late".to_string(),
            },
        )
        .expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "PublishedVerified(def456)");
                assert_eq!(event, "TaskCancelled");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_published_verified_rejects_attempt_recorded() {
        let err = apply(
            &TaskState::PublishedVerified {
                commit: "def456".to_string(),
            },
            &attempt_recorded(AttemptId::new(1)),
        )
        .expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "PublishedVerified(def456)");
                assert_eq!(event, "AttemptRecorded");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_paused_accepts_resumed_and_returns_to_resume_to() {
        let paused = TaskState::Paused {
            reason: PauseReason::Blocked,
            resume_to: Box::new(TaskState::Verifying {
                attempt: AttemptId::new(1),
            }),
        };
        let state = apply(&paused, &EventKind::Resumed).expect("legal");
        assert_eq!(
            state,
            TaskState::Verifying {
                attempt: AttemptId::new(1)
            }
        );
    }

    #[test]
    fn from_paused_rejects_resumed_when_the_reason_is_human_gate() {
        let paused = TaskState::Paused {
            reason: PauseReason::HumanGate,
            resume_to: Box::new(TaskState::Queued),
        };
        let err = apply(&paused, &EventKind::Resumed).expect_err("illegal: must ack instead");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Paused");
                assert_eq!(event, "Resumed");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_paused_accepts_gate_acknowledged_when_the_reason_is_human_gate() {
        let paused = TaskState::Paused {
            reason: PauseReason::HumanGate,
            resume_to: Box::new(TaskState::Queued),
        };
        let at = OffsetDateTime::UNIX_EPOCH;
        let state = apply(
            &paused,
            &EventKind::GateAcknowledged {
                by: "yalovoy".to_string(),
                at,
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Acknowledged {
                by: "yalovoy".to_string(),
                at,
            }
        );
    }

    #[test]
    fn from_paused_rejects_gate_acknowledged_when_the_reason_is_not_human_gate() {
        let paused = TaskState::Paused {
            reason: PauseReason::Blocked,
            resume_to: Box::new(TaskState::Queued),
        };
        let err = apply(
            &paused,
            &EventKind::GateAcknowledged {
                by: "yalovoy".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
        )
        .expect_err("illegal: nothing to acknowledge here");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Paused");
                assert_eq!(event, "GateAcknowledged");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_paused_recovery_decision_resume_returns_to_resume_to() {
        let paused = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Preflight),
        };
        let state = apply(
            &paused,
            &EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "journal complete through phase".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Preflight);
    }

    #[test]
    fn from_paused_recovery_decision_already_applied_returns_to_resume_to() {
        let paused = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Publishing {
                attempt: AttemptId::new(1),
            }),
        };
        let state = apply(
            &paused,
            &EventKind::RecoveryDecision {
                decision: Recovery::AlreadyApplied,
                detail: "push already landed on the remote".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(
            state,
            TaskState::Publishing {
                attempt: AttemptId::new(1)
            }
        );
    }

    #[test]
    fn from_paused_recovery_decision_mark_interrupted_stays_paused() {
        let paused = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Preflight),
        };
        let state = apply(
            &paused,
            &EventKind::RecoveryDecision {
                decision: Recovery::MarkInterrupted,
                detail: "worktree cannot be trusted".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, paused);
    }

    #[test]
    fn from_paused_rejects_recovery_decision_when_the_reason_is_not_interrupted() {
        let paused = TaskState::Paused {
            reason: PauseReason::Limit { until: None },
            resume_to: Box::new(TaskState::Preflight),
        };
        let err = apply(
            &paused,
            &EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "n/a".to_string(),
            },
        )
        .expect_err("illegal: recovery only follows an interruption");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Paused");
                assert_eq!(event, "RecoveryDecision");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    #[test]
    fn from_paused_accepts_task_cancelled() {
        let paused = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Queued),
        };
        let state = apply(
            &paused,
            &EventKind::TaskCancelled {
                reason: "no longer needed".to_string(),
            },
        )
        .expect("legal");
        assert_eq!(state, TaskState::Cancelled);
    }

    #[test]
    fn from_paused_rejects_preflight_started() {
        let paused = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Queued),
        };
        let err = apply(&paused, &EventKind::PreflightStarted).expect_err("illegal");
        match err {
            Error::InvalidTransition { from, event } => {
                assert_eq!(from, "Paused");
                assert_eq!(event, "PreflightStarted");
            }
            other => panic!("expected InvalidTransition, got {other:?}"),
        }
    }

    /// One representative [`TaskState`] per state the machine distinguishes
    /// for transition purposes, labeled for use in the cross-product test
    /// below. `Paused` is split by [`PauseReason`] because legality of
    /// `Resumed`, `GateAcknowledged` and `RecoveryDecision` depends on it.
    fn representative_states() -> Vec<(&'static str, TaskState)> {
        let attempt = AttemptId::new(1);
        vec![
            ("Queued", TaskState::Queued),
            ("Preflight", TaskState::Preflight),
            (
                "Running",
                TaskState::Running {
                    attempt,
                    phase: Phase::Implement,
                },
            ),
            (
                "Remediating",
                TaskState::Remediating {
                    attempt,
                    phase: Phase::Implement,
                },
            ),
            ("Verifying", TaskState::Verifying { attempt }),
            ("Publishing", TaskState::Publishing { attempt }),
            (
                "PublishedVerified",
                TaskState::PublishedVerified {
                    commit: "abc123".to_string(),
                },
            ),
            ("Done", TaskState::Done),
            (
                "Acknowledged",
                TaskState::Acknowledged {
                    by: "alice".to_string(),
                    at: OffsetDateTime::UNIX_EPOCH,
                },
            ),
            (
                "Paused/Limit",
                TaskState::Paused {
                    reason: PauseReason::Limit { until: None },
                    resume_to: Box::new(TaskState::Queued),
                },
            ),
            (
                "Paused/Input",
                TaskState::Paused {
                    reason: PauseReason::Input,
                    resume_to: Box::new(TaskState::Queued),
                },
            ),
            (
                "Paused/HumanGate",
                TaskState::Paused {
                    reason: PauseReason::HumanGate,
                    resume_to: Box::new(TaskState::Queued),
                },
            ),
            (
                "Paused/Interrupted",
                TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    resume_to: Box::new(TaskState::Preflight),
                },
            ),
            (
                "Paused/Blocked",
                TaskState::Paused {
                    reason: PauseReason::Blocked,
                    resume_to: Box::new(TaskState::Queued),
                },
            ),
            (
                "Failed",
                TaskState::Failed {
                    class: FailureClass::AgentFailure,
                    detail: "boom".to_string(),
                },
            ),
            ("Cancelled", TaskState::Cancelled),
        ]
    }

    /// One representative [`EventKind`] per variant the machine
    /// distinguishes for transition purposes. Field values are arbitrary:
    /// `apply` never inspects a payload to decide legality, only the event's
    /// discriminant and (for `Paused`) the current state's `PauseReason`.
    fn representative_events() -> Vec<EventKind> {
        let attempt = AttemptId::new(1);
        vec![
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "disk full".to_string(),
            },
            attempt_started(attempt),
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Implement,
            },
            EventKind::AgentOutput {
                attempt,
                stream: Stream::Stdout,
                text: "x".to_string(),
            },
            EventKind::VerifyPassed { attempt },
            EventKind::VerifyFailed {
                attempt,
                class: FailureClass::VerificationFailure,
                detail: "x".to_string(),
            },
            EventKind::PublishStarted {
                attempt,
                candidate_sha: "def".to_string(),
            },
            EventKind::PublishVerified {
                commit: "def".to_string(),
                remote_sha: "def".to_string(),
            },
            // Matches the commit `representative_states` gives
            // `PublishedVerified`, so the one legal (state, event) pair for
            // `TaskDone` in `ALLOWED` below stays legal now that
            // `from_published_verified` checks the payload commit.
            EventKind::TaskDone {
                commit: "abc123".to_string(),
            },
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "x".to_string(),
            },
            EventKind::RetryStarted { attempt },
            EventKind::TaskCancelled {
                reason: "x".to_string(),
            },
            EventKind::Paused {
                reason: PauseReason::Blocked,
            },
            EventKind::Resumed,
            EventKind::Interrupted {
                phase: Phase::Implement,
            },
            EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "x".to_string(),
            },
            EventKind::TddExceptionUsed {
                exception: TddException::Documentation,
                reason: "x".to_string(),
            },
            EventKind::GateAcknowledged {
                by: "x".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
            attempt_recorded(attempt),
            EventKind::DecisionRaised {
                request: crate::DecisionRequest {
                    question: "q?".to_string(),
                    options: vec!["a".to_string(), "b".to_string()],
                    tradeoffs: "t".to_string(),
                    impact: "i".to_string(),
                    recommended: None,
                },
            },
            decision_resolved(),
            self_healing_report(attempt),
        ]
    }

    #[test]
    fn every_state_event_pair_is_either_allowed_or_rejected_as_invalid_transition() {
        // The complete list of legal (state, event) pairs, by label. Any
        // pair not listed here must make `apply` return
        // `Error::InvalidTransition`: adding a new legal transition means
        // deliberately adding a line here, not just an arm in `apply`.
        const ALLOWED: &[(&str, &str)] = &[
            ("Queued", "TaskQueued"),
            ("Queued", "PreflightStarted"),
            ("Queued", "Paused"),
            ("Queued", "TaskCancelled"),
            ("Preflight", "PreflightPassed"),
            ("Preflight", "PreflightFailed"),
            ("Preflight", "AttemptStarted"),
            ("Preflight", "Paused"),
            ("Preflight", "Interrupted"),
            ("Preflight", "TaskCancelled"),
            ("Running", "PhaseEntered"),
            ("Running", "AgentOutput"),
            ("Running", "AttemptRecorded"),
            ("Running", "TddExceptionUsed"),
            ("Running", "TaskFailed"),
            ("Running", "DecisionRaised"),
            ("Running", "Paused"),
            ("Running", "Interrupted"),
            ("Running", "TaskCancelled"),
            ("Remediating", "PhaseEntered"),
            ("Remediating", "AgentOutput"),
            ("Remediating", "AttemptRecorded"),
            ("Remediating", "TddExceptionUsed"),
            ("Remediating", "SelfHealingReport"),
            ("Remediating", "TaskFailed"),
            ("Remediating", "DecisionRaised"),
            ("Remediating", "Paused"),
            ("Remediating", "Interrupted"),
            ("Remediating", "TaskCancelled"),
            ("Verifying", "VerifyPassed"),
            ("Verifying", "VerifyFailed"),
            ("Verifying", "PublishStarted"),
            ("Verifying", "AttemptRecorded"),
            ("Verifying", "Paused"),
            ("Verifying", "Interrupted"),
            ("Verifying", "TaskCancelled"),
            ("Publishing", "PublishVerified"),
            ("Publishing", "AttemptRecorded"),
            ("Publishing", "TaskFailed"),
            ("Publishing", "Paused"),
            ("Publishing", "Interrupted"),
            ("PublishedVerified", "TaskDone"),
            ("Paused/Limit", "Resumed"),
            ("Paused/Limit", "TaskCancelled"),
            ("Paused/Input", "Resumed"),
            ("Paused/Input", "DecisionResolved"),
            ("Paused/Input", "TaskCancelled"),
            ("Paused/HumanGate", "GateAcknowledged"),
            ("Paused/HumanGate", "TaskCancelled"),
            ("Paused/Interrupted", "Resumed"),
            ("Paused/Interrupted", "RecoveryDecision"),
            ("Paused/Interrupted", "TaskCancelled"),
            ("Paused/Blocked", "Resumed"),
            ("Paused/Blocked", "TaskCancelled"),
            ("Failed", "RetryStarted"),
            ("Failed", "TaskCancelled"),
            // Done, Acknowledged and Cancelled are terminal: no event is
            // legal against them, so they contribute no rows. Failed is
            // terminal too, except that a human's `retry` or `cancel` may
            // leave it.
        ];

        let states = representative_states();
        let events = representative_events();
        assert_eq!(states.len(), 16, "expected one row per distinguished state");
        assert_eq!(events.len(), 25, "expected one row per distinguished event");

        let mut checked = 0;
        for (state_label, state) in &states {
            for event in &events {
                let event_label = event.discriminant();
                let allowed = ALLOWED.contains(&(*state_label, event_label));
                let result = apply(state, event);
                if allowed {
                    assert!(
                        result.is_ok(),
                        "expected {state_label} to accept {event_label}, got {result:?}"
                    );
                } else {
                    match result {
                        Err(Error::InvalidTransition { .. }) => {}
                        other => panic!(
                            "expected {state_label} to reject {event_label} with \
                             InvalidTransition, got {other:?}"
                        ),
                    }
                }
                checked += 1;
            }
        }
        assert_eq!(checked, states.len() * events.len());
        assert_eq!(
            ALLOWED.len(),
            56,
            "the allowed list itself changed size; update this guard deliberately"
        );
    }

    #[test]
    fn check_one_active_rejects_two_active_tasks_naming_both() {
        let mut states = BTreeMap::new();
        states.insert(
            TaskId::new(1),
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
        );
        states.insert(
            TaskId::new(2),
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
        );

        let err = check_one_active(&states).expect_err("two active tasks must be rejected");
        match err {
            Error::Policy { detail, .. } => {
                assert!(detail.contains('1'), "detail should name task 1: {detail}");
                assert!(detail.contains('2'), "detail should name task 2: {detail}");
            }
            other => panic!("expected Policy, got {other:?}"),
        }
    }

    #[test]
    fn check_one_active_allows_one_active_task_alongside_several_paused() {
        let mut states = BTreeMap::new();
        states.insert(
            TaskId::new(1),
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
        );
        states.insert(
            TaskId::new(2),
            TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Queued),
            },
        );
        states.insert(
            TaskId::new(3),
            TaskState::Paused {
                reason: PauseReason::Limit { until: None },
                resume_to: Box::new(TaskState::Preflight),
            },
        );
        states.insert(TaskId::new(4), TaskState::Queued);

        check_one_active(&states).expect("one active task with several paused/queued is fine");
    }

    #[test]
    fn check_one_active_allows_zero_active_tasks() {
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Queued);
        states.insert(
            TaskId::new(2),
            TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Queued),
            },
        );

        check_one_active(&states).expect("no active tasks at all is fine");
    }

    #[test]
    fn check_predecessor_blocks_on_a_pending_predecessor_and_names_it() {
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Queued);

        let err =
            check_predecessor(&states, TaskId::new(3)).expect_err("task 2 has not been published");
        match err {
            Error::Policy { detail, .. } => {
                assert!(
                    detail.contains('2'),
                    "detail should name the blocking predecessor: {detail}"
                );
            }
            other => panic!("expected Error::Policy, got {other:?}"),
        }
    }

    #[test]
    fn check_predecessor_allows_done_cancelled_and_published_verified_predecessors() {
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Cancelled);
        states.insert(
            TaskId::new(3),
            TaskState::PublishedVerified {
                commit: "abc123".to_string(),
            },
        );

        check_predecessor(&states, TaskId::new(4))
            .expect("done, cancelled and published-verified predecessors are all settled");
    }

    #[test]
    fn check_predecessor_allows_an_acknowledged_gate_predecessor() {
        let mut states = BTreeMap::new();
        states.insert(
            TaskId::new(1),
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
        );

        check_predecessor(&states, TaskId::new(2))
            .expect("an acknowledged gate is settled, like Done or PublishedVerified");
    }

    #[test]
    fn check_predecessor_ignores_tasks_at_or_after_next() {
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Queued);

        check_predecessor(&states, TaskId::new(2))
            .expect("a task is not its own predecessor, and later tasks don't block it either");
    }

    #[test]
    fn check_predecessor_allows_a_task_with_no_predecessors() {
        let states: BTreeMap<TaskId, TaskState> = BTreeMap::new();
        check_predecessor(&states, TaskId::new(1)).expect("no predecessors, nothing to block on");
    }

    #[test]
    fn apply_rejects_every_event_against_every_terminal_state() {
        let terminals = vec![
            TaskState::Done,
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "crashed".to_string(),
            },
            TaskState::Cancelled,
        ];

        for state in terminals {
            let err =
                apply(&state, &EventKind::Resumed).expect_err("terminal states accept nothing");
            match err {
                Error::InvalidTransition { from, event } => {
                    assert_eq!(from, state.name());
                    assert_eq!(event, "Resumed");
                }
                other => panic!("expected InvalidTransition, got {other:?}"),
            }
        }
    }
}

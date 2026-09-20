//! The vocabulary of a run: which phase it is in, why it stopped, and what
//! recovery decided.
//!
//! [`TaskState`] is the lifecycle those words describe; [`Phase`],
//! [`PauseReason`], [`Stream`] and [`Recovery`] are the words it is spoken with.
//!
//! [`apply`] is the behaviour, and the only place a task moves. Nothing here
//! holds a lock, reads a clock or touches a file, which is what lets `apply`,
//! the event catalog that carries these states, and every screen that displays
//! a run all speak the same words without any of them owning the others.
//!
//! The variants are exactly those `docs/DESIGN.md` fixes, spelled as it spells
//! them, because they are what the journal stores and what `--json` output
//! prints: a renamed or added variant silently changes durable data, so the
//! tests below pin the count and the encoding of each.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::classify::FailureClass;
use crate::error::{Error, Result};
use crate::event::EventKind;
use crate::ids::AttemptId;
use crate::ids::TaskId;

/// Where a task stands in its lifecycle, and everything the last journal
/// record proved about it.
///
/// `Queued` → `Preflight` → `Running` → `Verifying` → `Publishing` →
/// `PublishedVerified` → `Done` is the path a task takes when nothing goes
/// wrong; [`TaskState::Remediating`] stands in for `Running` during the one
/// bounded second attempt a failed task is allowed, and
/// [`TaskState::Paused`] stands in wherever a run has to stop and wait. The
/// payloads are the reason this is one enum and not a name: a state that could
/// not say which attempt it belongs to, or which commit the remote was read
/// back holding, would leave the journal as the only place those facts lived,
/// and `state::apply` can stay pure — no I/O, no clock — only because the
/// state carries them.
///
/// This is durable data. The journal stores it in `serde`'s default
/// representation and `--json` output prints it, so the variants and their
/// field names are exactly the ones the `docs/DESIGN.md` Core types section
/// writes, and a later task cannot reshape one in passing without rewriting
/// every journal written before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskState {
    /// In the queue and not yet started: the state a parsed plan lands in.
    Queued,
    /// Running the checks that must hold before an agent is let near the tree.
    Preflight,
    /// An agent is working: this `attempt`, on this `phase`.
    Running {
        /// The attempt now running.
        attempt: AttemptId,
        /// The protocol step it is on.
        phase: Phase,
    },
    /// The second, bounded attempt at a task that failed once, carrying the
    /// failure bundle rather than a fresh start.
    Remediating {
        /// The remediation attempt, which is always later than the one that failed.
        attempt: AttemptId,
        /// The protocol step the remediation is on.
        phase: Phase,
    },
    /// The agent has finished; the gates are deciding whether that counts.
    Verifying {
        /// The attempt whose output is being verified.
        attempt: AttemptId,
    },
    /// The gates passed; the commit is being made and pushed.
    Publishing {
        /// The attempt being published.
        attempt: AttemptId,
    },
    /// The remote has been read back and holds `commit`. Work is not published
    /// until this state exists, because nothing is taken on an agent's say-so.
    PublishedVerified {
        /// The commit the remote was proved to hold.
        commit: String,
    },
    /// Finished, with its outcome recorded.
    Done,
    /// Finished, and a human has said so: what history records as closed
    /// rather than merely complete.
    Acknowledged {
        /// Who acknowledged it.
        by: String,
        /// When they did.
        at: OffsetDateTime,
    },
    /// Standing still for a reason outside the work, and holding where to go
    /// back to. The boxed state is what makes a pause durable: a supervisor
    /// that dies mid-pause resumes that same wait rather than guessing one.
    Paused {
        /// Why the run stopped.
        reason: PauseReason,
        /// The state to return to. Inside the pause, because a pause without
        /// one is not resumable, only abandoned.
        resume_to: Box<TaskState>,
    },
    /// Finished, and not done, classified by the response it needs.
    Failed {
        /// What kind of failure this is, which is what the next move is chosen
        /// from.
        class: FailureClass,
        /// The sentence that goes beside the class, never in place of it.
        detail: String,
    },
    /// Dropped by a human decision, so the queue may proceed past it.
    Cancelled,
}

impl TaskState {
    /// Whether this state offers the supervisor anywhere further to go.
    ///
    /// Unlike [`TaskState::Paused`], none of the four states this returns
    /// `true` for holds a state to return to: the queue has already decided
    /// what to do about them — proceed past a [`TaskState::Cancelled`] task,
    /// stop the run at a [`TaskState::Failed`] one, which is the "terminal
    /// failure" `docs/CONTRACT.md` says a `run` stops at. Putting one back in
    /// motion is a human command that starts *new* work: `retry` begins a
    /// fresh remediation attempt rather than continuing the failed one, so the
    /// terminal state is the record an attempt was seeded from, not a place a
    /// transition leaves. A [`TaskState::Paused`] task is the contrast case,
    /// and stays non-terminal even above a terminal `resume_to`: it is waiting,
    /// not finished, and the type says so.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done | Self::Acknowledged { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }

    /// Whether the task is standing still with somewhere to come back to.
    ///
    /// Exactly [`TaskState::Paused`], regardless of what it boxes: a pause
    /// above a finished state is still a pause, which is why the answer is not
    /// read out of the boxed state.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. })
    }

    /// The variant's own name, spelled as `docs/DESIGN.md` spells it.
    ///
    /// This is the identity of the state, not a rendering of it: it is what
    /// `state::apply` hands to `Error::InvalidTransition { from }` to say which
    /// state refused an event, and what a screen prints beside the payload. The
    /// lowercase, hyphenated forms `--json` output uses belong to whoever is
    /// printing, so they are not here.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Preflight => "Preflight",
            Self::Running { .. } => "Running",
            Self::Remediating { .. } => "Remediating",
            Self::Verifying { .. } => "Verifying",
            Self::Publishing { .. } => "Publishing",
            Self::PublishedVerified { .. } => "PublishedVerified",
            Self::Done => "Done",
            Self::Acknowledged { .. } => "Acknowledged",
            Self::Paused { .. } => "Paused",
            Self::Failed { .. } => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// The step a work protocol is on, as the queue and inspector display it.
///
/// One enum serves all three v1 protocols, so it carries steps that any single
/// protocol never reaches: `Implement` belongs to `direct`, the
/// red/green/refactor trio to `tdd`, and `Goal`, `Scope`, `AcceptanceTests`,
/// `Review`, `Harden` and `DoneCheck` to `spec-first`, which is why no
/// `SpecFirst` variant exists. `Verify` and `Publish` belong to every protocol:
/// no protocol may end without them.
///
/// Declared in no ordering that means anything — the protocol decides the
/// sequence — so this derives no [`Ord`]: comparing two phases for less-than
/// would claim a fact only the protocol in force can supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// `spec-first`: what the task is for, written down before anything is.
    Goal,
    /// `spec-first`: which files the work may reach, and which it may not.
    Scope,
    /// `spec-first`: the tests that must pass before the phase counts as done.
    AcceptanceTests,
    /// `direct`: the one implementation phase that protocol allows.
    Implement,
    /// `tdd`: tests only; production paths stay read-only, and the new test
    /// must fail.
    Red,
    /// `tdd`: implementation changes are permitted, and the new test must pass.
    Green,
    /// `tdd`: cleanup, while the targeted tests stay green.
    Refactor,
    /// `spec-first`: the work read critically, by a reviewer if one is configured.
    Review,
    /// `spec-first`: robustness work the acceptance tests do not demand.
    Harden,
    /// `spec-first`: the completion checks, run before the gates rather than as one.
    DoneCheck,
    /// The mandatory verification gates. Every protocol ends here.
    Verify,
    /// Commit, push, and prove the remote holds the commit.
    Publish,
}

/// Why a run is standing still, and what it is waiting for.
///
/// The `until` of [`PauseReason::Limit`] is the only field any of this
/// vocabulary carries, which is why this is the one enum here that is `Clone`
/// rather than [`Copy`]. The instant is what makes a limit wait resumable after
/// a restart: the journal holds it, so a supervisor that died mid-wake resumes
/// to the same wait rather than a shorter one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// A provider refused work. `until` is the reset time when one was
    /// advertised, and [`None`] when it was not.
    Limit {
        /// The instant to resume at, if the provider said when.
        until: Option<OffsetDateTime>,
    },
    /// Stopped on a question only a human can answer.
    Input,
    /// Stopped at a gate that waits for a human decision.
    HumanGate,
    /// Stopped by a signal or a crash; recovery decides where to resume.
    Interrupted,
    /// Stopped by something outside the supervisor that it cannot fix: a dirty
    /// tree, a missing tool, no remote.
    Blocked,
}

/// Which of an agent's two output streams a line arrived on.
///
/// Kept from the moment a line is read, because a screen that shows only the
/// agent's own words and a screen that shows its complaints are different
/// questions, and the distinction is unrecoverable after the two are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    /// What the agent printed to be read as its work.
    Stdout,
    /// What the agent printed to be read as a problem.
    Stderr,
}

/// What recovery concluded about an interrupted run.
///
/// The three answers are the only ones a journal replay can give: the work was
/// not started, was started and unappliable, or was applied and already
/// recorded. Anything else is a question, not a recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovery {
    /// The journal ends mid-phase and nothing was published: resume the phase.
    Resume,
    /// The run cannot be resumed safely, so record it as interrupted and stop.
    MarkInterrupted,
    /// The transition being replayed is already in the journal: apply nothing.
    AlreadyApplied,
}

/// The half of the pipeline a [`Phase`] belongs to.
///
/// Two of the twelve phases are the machine's own work — the gates, and the
/// publication — and the other ten are an agent's. A transition needs to know
/// which half an entered phase sits in and needs nothing else from it, so the
/// question is answered once here rather than ten times across the helpers
/// below. [`Phase`] is matched exhaustively: a new phase has to be placed in a
/// half, not defaulted into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseEntry {
    /// An agent is working: the state holds the attempt and the phase it entered.
    Work,
    /// The gates are deciding whether the attempt counts.
    Gates,
    /// The commit is being made and pushed, which only a passed gate opens.
    Publication,
}

/// Which half of the pipeline `phase` belongs to.
fn phase_entry(phase: Phase) -> PhaseEntry {
    match phase {
        Phase::Verify => PhaseEntry::Gates,
        Phase::Publish => PhaseEntry::Publication,
        Phase::Goal
        | Phase::Scope
        | Phase::AcceptanceTests
        | Phase::Implement
        | Phase::Red
        | Phase::Green
        | Phase::Refactor
        | Phase::Review
        | Phase::Harden
        | Phase::DoneCheck => PhaseEntry::Work,
    }
}

/// The refusal every rejected move returns: one error, naming the state that
/// refused and the event it refused.
///
/// Both names are always at hand — a helper holds the state it was dispatched
/// for and the event it was handed — so a refusal is never vaguer for having
/// been built in a shared helper.
fn refused(from: &str, event: &EventKind) -> Error {
    Error::InvalidTransition {
        from: from.to_owned(),
        event: event.discriminant().to_owned(),
    }
}

/// `to` when `holds`, and the refusal of `event` by `from` when it does not.
///
/// A move that is legal only for the attempt, commit or phase it names ends in
/// the same error as one that is never legal there, so the condition is the
/// only part of the arm worth spelling at the call site.
fn refuse_unless(holds: bool, to: TaskState, from: &str, event: &EventKind) -> Result<TaskState> {
    holds.then_some(to).ok_or_else(|| refused(from, event))
}

/// `state`, parked for `reason`, holding itself as where to come back to.
///
/// Every state but the four terminal ones can be parked this way, which is why
/// no call site has to work out a `resume_to`: what a pause resumes to is
/// exactly where the run was standing when it stopped.
fn parked(state: TaskState, reason: PauseReason) -> TaskState {
    TaskState::Paused {
        reason,
        resume_to: Box::new(state),
    }
}

/// The one place a task's state changes.
///
/// One pure call: a state and an event in, the next state out, with no I/O, no
/// clock and no randomness to make the same pair answer differently twice. That
/// is what lets the journal, the recovery walk and a screen agree on what a run
/// did — each folds the same events over the same states and gets the same
/// answer back, which is the whole of "an interruption resolves to a known
/// state" (VISION.md §6).
///
/// The dispatcher matches on `state` alone and hands the event to one function
/// per state, so what an event means is always answered by the state that was
/// asked. The four terminal states are refused here rather than in helpers of
/// their own: they accept no event, and nothing a caller can send changes that.
///
/// # Errors
///
/// [`Error::InvalidTransition`] naming the state that refused and the event it
/// refused, for every pair the table does not allow. Each state says what every
/// event means there and lists the events it refuses by name, so a new
/// [`EventKind`] variant fails to build until it has been answered in all eight
/// states, and a new [`TaskState`] variant fails to build until it has a
/// helper. That is the point of the decomposition (ADR-0022).
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
        TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled => Err(refused(state.name(), event)),
    }
}

/// What an event means to a task still sitting in the queue.
///
/// `TaskQueued` is the event that put the task here, and a state has to accept
/// its own seed: recovery folds a journal from the start, so replaying the seed
/// onto `Queued` is ordinary rather than an error. A recovery verdict that
/// resumes the run, or that found its work already applied, tells a queued task
/// nothing it has not already done, so it is answered by the same arm.
///
/// Nothing else has happened yet — the checks have not begun, so no attempt,
/// phase, verdict or commit can be named — and there are exactly three ways
/// out: preflight starts, the run waits, or a human drops the task.
fn from_queued(event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "Queued";
    match event {
        EventKind::TaskQueued { .. }
        | EventKind::RecoveryDecision {
            decision: Recovery::Resume | Recovery::AlreadyApplied,
            ..
        } => Ok(TaskState::Queued),
        EventKind::PreflightStarted => Ok(TaskState::Preflight),
        EventKind::Paused { reason } => Ok(parked(TaskState::Queued, reason.clone())),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::RecoveryDecision {
            decision: Recovery::MarkInterrupted,
            ..
        } => Err(refused(FROM, event)),
    }
}

/// What an event means to a task whose checks are running.
///
/// Preflight's findings are the base commit, the provider and the free disk;
/// `TaskState::Preflight` holds none of them, because `docs/DESIGN.md` gives it
/// no field to hold them in. So the events that record a check having finished
/// leave the task where it is — the journal keeps the finding — and so does a
/// recovery verdict, which cannot resume work that has not started. Only the
/// two things a state can express end it: a refusal, and an attempt entering a
/// phase.
fn from_preflight(event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "Preflight";
    match event {
        EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::RecoveryDecision {
            decision: Recovery::Resume | Recovery::AlreadyApplied,
            ..
        } => Ok(TaskState::Preflight),
        EventKind::PreflightFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::PhaseEntered { attempt, phase } => match phase_entry(*phase) {
            PhaseEntry::Work => Ok(TaskState::Running {
                attempt: *attempt,
                phase: *phase,
            }),
            PhaseEntry::Gates => Ok(TaskState::Verifying { attempt: *attempt }),
            PhaseEntry::Publication => Err(refused(FROM, event)),
        },
        EventKind::Paused { reason } => Ok(parked(TaskState::Preflight, reason.clone())),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::RecoveryDecision {
            decision: Recovery::MarkInterrupted,
            ..
        } => Err(refused(FROM, event)),
    }
}
/// What an event means to an attempt that is working.
///
/// Two facts make an event belong here: it names this attempt, and it does not
/// claim the task has finished by any route other than the gates. A later
/// attempt may be recorded but not adopted — `Running` holds one attempt, so
/// `AttemptStarted` for a later one leaves the state holding the earlier one,
/// which is what the journal already knew.
///
/// A failed gate is a self-transition rather than a failure. Whether a second
/// attempt is still allowed is `max_attempts` and `max_remediation_attempts`,
/// which are configuration this pure function cannot read, so deciding that
/// here would bake a limit into the machine that owns none: the runner counts
/// and either journals a later attempt or journals `TaskFailed`.
///
/// An attempt's record is a third kind of event: evidence rather than a
/// transition. It names the attempt the state is holding and moves nothing, so
/// filing it leaves the run where it was — which is also what makes a replay of
/// a journal that already holds the record change nothing.
fn from_running(attempt: AttemptId, phase: Phase, event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "Running";
    match event {
        EventKind::AttemptStarted { attempt: later, .. } => refuse_unless(
            *later > attempt,
            TaskState::Running { attempt, phase },
            FROM,
            event,
        ),
        EventKind::PhaseEntered {
            attempt: entered,
            phase: next,
        } => match phase_entry(*next) {
            PhaseEntry::Work => match entered.cmp(&attempt) {
                Ordering::Equal => Ok(TaskState::Running {
                    attempt,
                    phase: *next,
                }),
                Ordering::Greater => Ok(TaskState::Remediating {
                    attempt: *entered,
                    phase: *next,
                }),
                Ordering::Less => Err(refused(FROM, event)),
            },
            PhaseEntry::Gates => refuse_unless(
                *entered >= attempt,
                TaskState::Verifying { attempt: *entered },
                FROM,
                event,
            ),
            PhaseEntry::Publication => Err(refused(FROM, event)),
        },
        EventKind::AgentOutput { attempt: mine, .. }
        | EventKind::VerifyFailed { attempt: mine, .. } => refuse_unless(
            *mine == attempt,
            TaskState::Running { attempt, phase },
            FROM,
            event,
        ),
        EventKind::AttemptRecorded { record } => refuse_unless(
            record.id == attempt,
            TaskState::Running { attempt, phase },
            FROM,
            event,
        ),
        EventKind::VerifyPassed { attempt: mine } => refuse_unless(
            *mine == attempt,
            TaskState::Publishing { attempt },
            FROM,
            event,
        ),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::Paused { reason } => Ok(parked(
            TaskState::Running { attempt, phase },
            reason.clone(),
        )),
        EventKind::Interrupted { phase: lost } => refuse_unless(
            *lost == phase,
            parked(
                TaskState::Running { attempt, phase },
                PauseReason::Interrupted,
            ),
            FROM,
            event,
        ),
        EventKind::RecoveryDecision { decision, .. } => match decision {
            Recovery::Resume | Recovery::AlreadyApplied => {
                Ok(TaskState::Running { attempt, phase })
            }
            Recovery::MarkInterrupted => Ok(parked(
                TaskState::Running { attempt, phase },
                PauseReason::Interrupted,
            )),
        },
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::GateAcknowledged { .. } => Err(refused(FROM, event)),
    }
}

/// What an event means to the bounded second attempt at a task that failed
/// once.
///
/// The rules are `Running`'s with one difference: an attempt already inside a
/// remediation stays inside it, so entering a phase on the same attempt moves
/// from `Remediating` to `Remediating` rather than back to `Running`. A task
/// that has begun remediating cannot present itself as working again; which
/// attempt the machine is still allowed to start is the runner's count, not
/// this function's, exactly as it is in `Running`.
fn from_remediating(attempt: AttemptId, phase: Phase, event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "Remediating";
    match event {
        EventKind::AttemptStarted { attempt: later, .. } => refuse_unless(
            *later > attempt,
            TaskState::Remediating { attempt, phase },
            FROM,
            event,
        ),
        EventKind::PhaseEntered {
            attempt: entered,
            phase: next,
        } => match phase_entry(*next) {
            PhaseEntry::Work => match entered.cmp(&attempt) {
                Ordering::Equal | Ordering::Greater => Ok(TaskState::Remediating {
                    attempt: *entered,
                    phase: *next,
                }),
                Ordering::Less => Err(refused(FROM, event)),
            },
            PhaseEntry::Gates => refuse_unless(
                *entered >= attempt,
                TaskState::Verifying { attempt: *entered },
                FROM,
                event,
            ),
            PhaseEntry::Publication => Err(refused(FROM, event)),
        },
        EventKind::AgentOutput { attempt: mine, .. }
        | EventKind::VerifyFailed { attempt: mine, .. } => refuse_unless(
            *mine == attempt,
            TaskState::Remediating { attempt, phase },
            FROM,
            event,
        ),
        EventKind::AttemptRecorded { record } => refuse_unless(
            record.id == attempt,
            TaskState::Remediating { attempt, phase },
            FROM,
            event,
        ),
        EventKind::VerifyPassed { attempt: mine } => refuse_unless(
            *mine == attempt,
            TaskState::Publishing { attempt },
            FROM,
            event,
        ),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::Paused { reason } => Ok(parked(
            TaskState::Remediating { attempt, phase },
            reason.clone(),
        )),
        EventKind::Interrupted { phase: lost } => refuse_unless(
            *lost == phase,
            parked(
                TaskState::Remediating { attempt, phase },
                PauseReason::Interrupted,
            ),
            FROM,
            event,
        ),
        EventKind::RecoveryDecision { decision, .. } => match decision {
            Recovery::Resume | Recovery::AlreadyApplied => {
                Ok(TaskState::Remediating { attempt, phase })
            }
            Recovery::MarkInterrupted => Ok(parked(
                TaskState::Remediating { attempt, phase },
                PauseReason::Interrupted,
            )),
        },
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::GateAcknowledged { .. } => Err(refused(FROM, event)),
    }
}

/// What an event means while the gates are deciding whether an attempt counts.
///
/// `Verifying` holds an attempt and no phase, so an event naming an attempt is
/// answered by the state itself and [`EventKind::Interrupted`] is not: the
/// phase the journal ended in is recovery's memory of where the supervisor
/// died, not a fact this state can contradict. An agent's output is refused
/// outright — by definition the attempt finished before the gates opened, so
/// output arriving now belongs to no attempt this task is running.
fn from_verifying(attempt: AttemptId, event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "Verifying";
    match event {
        EventKind::AttemptStarted { attempt: later, .. } => refuse_unless(
            *later > attempt,
            TaskState::Verifying { attempt },
            FROM,
            event,
        ),
        EventKind::PhaseEntered {
            attempt: entered,
            phase: next,
        } => match phase_entry(*next) {
            PhaseEntry::Work => refuse_unless(
                *entered > attempt,
                TaskState::Remediating {
                    attempt: *entered,
                    phase: *next,
                },
                FROM,
                event,
            ),
            PhaseEntry::Gates => refuse_unless(
                *entered >= attempt,
                TaskState::Verifying { attempt: *entered },
                FROM,
                event,
            ),
            PhaseEntry::Publication => Err(refused(FROM, event)),
        },
        EventKind::VerifyPassed { attempt: mine } => refuse_unless(
            *mine == attempt,
            TaskState::Publishing { attempt },
            FROM,
            event,
        ),
        EventKind::VerifyFailed { attempt: mine, .. } => refuse_unless(
            *mine == attempt,
            TaskState::Verifying { attempt },
            FROM,
            event,
        ),
        EventKind::AttemptRecorded { record } => refuse_unless(
            record.id == attempt,
            TaskState::Verifying { attempt },
            FROM,
            event,
        ),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::Paused { reason } => {
            Ok(parked(TaskState::Verifying { attempt }, reason.clone()))
        }
        EventKind::Interrupted { .. } => Ok(parked(
            TaskState::Verifying { attempt },
            PauseReason::Interrupted,
        )),
        EventKind::RecoveryDecision { decision, .. } => match decision {
            Recovery::Resume | Recovery::AlreadyApplied => Ok(TaskState::Verifying { attempt }),
            Recovery::MarkInterrupted => Ok(parked(
                TaskState::Verifying { attempt },
                PauseReason::Interrupted,
            )),
        },
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::GateAcknowledged { .. } => Err(refused(FROM, event)),
    }
}
/// What an event means while the commit is being made and pushed.
///
/// This is the dangerous phase (VISION.md §6): the work exists locally and the
/// remote may or may not know it yet. So the state accepts the two facts that
/// were already true when it was entered — the gate verdict and the start of
/// the push — and moves on exactly one: [`EventKind::PublishVerified`], whose
/// two fields name the same commit. A push that is not yet proved by a fetched
/// remote is a push that has not happened, which is invariant 2 in the one
/// place it is easiest to break.
fn from_publishing(attempt: AttemptId, event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "Publishing";
    match event {
        EventKind::AttemptStarted { attempt: later, .. } => refuse_unless(
            *later > attempt,
            TaskState::Publishing { attempt },
            FROM,
            event,
        ),
        EventKind::PhaseEntered {
            attempt: entered,
            phase: next,
        } => match phase_entry(*next) {
            PhaseEntry::Work => refuse_unless(
                *entered > attempt,
                TaskState::Remediating {
                    attempt: *entered,
                    phase: *next,
                },
                FROM,
                event,
            ),
            PhaseEntry::Gates => refuse_unless(
                *entered > attempt,
                TaskState::Verifying { attempt: *entered },
                FROM,
                event,
            ),
            PhaseEntry::Publication => Err(refused(FROM, event)),
        },
        EventKind::VerifyPassed { attempt: mine }
        | EventKind::PublishStarted { attempt: mine, .. } => refuse_unless(
            *mine == attempt,
            TaskState::Publishing { attempt },
            FROM,
            event,
        ),
        EventKind::AttemptRecorded { record } => refuse_unless(
            record.id == attempt,
            TaskState::Publishing { attempt },
            FROM,
            event,
        ),
        EventKind::PublishVerified {
            commit: proved,
            remote_sha,
        } => refuse_unless(
            proved == remote_sha,
            TaskState::PublishedVerified {
                commit: proved.clone(),
            },
            FROM,
            event,
        ),
        EventKind::TaskFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::Paused { reason } => {
            Ok(parked(TaskState::Publishing { attempt }, reason.clone()))
        }
        EventKind::Interrupted { .. } => Ok(parked(
            TaskState::Publishing { attempt },
            PauseReason::Interrupted,
        )),
        EventKind::RecoveryDecision { decision, .. } => match decision {
            Recovery::Resume | Recovery::AlreadyApplied => Ok(TaskState::Publishing { attempt }),
            Recovery::MarkInterrupted => Ok(parked(
                TaskState::Publishing { attempt },
                PauseReason::Interrupted,
            )),
        },
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::GateAcknowledged { .. } => Err(refused(FROM, event)),
    }
}

/// What an event means to work the remote has been read back holding.
///
/// The commit is the whole of the state's authority, so it is the answer to
/// every event that names one: a second reading of the remote that agrees
/// changes nothing, and a `TaskDone` for any other commit is a claim about
/// work that was never published. Nothing else is accepted — not a failure,
/// not a cancellation, not a gate — because the work is already where it was
/// meant to get, and a state that proved that cannot unprove it.
///
/// An attempt's record belongs to the attempt that produced it, and by here
/// that attempt is filed and finished: `Publishing` is where its evidence
/// belongs, and this state holds no attempt to name one against.
fn from_published_verified(commit: &str, event: &EventKind) -> Result<TaskState> {
    const FROM: &str = "PublishedVerified";
    match event {
        EventKind::PublishVerified {
            commit: proved,
            remote_sha,
        } => refuse_unless(
            proved == commit && remote_sha == commit,
            TaskState::PublishedVerified {
                commit: commit.to_owned(),
            },
            FROM,
            event,
        ),
        EventKind::TaskDone { commit: closed } => {
            refuse_unless(closed == commit, TaskState::Done, FROM, event)
        }
        EventKind::Paused { reason } => Ok(parked(
            TaskState::PublishedVerified {
                commit: commit.to_owned(),
            },
            reason.clone(),
        )),
        EventKind::RecoveryDecision { decision, .. } => match decision {
            Recovery::Resume | Recovery::AlreadyApplied => Ok(TaskState::PublishedVerified {
                commit: commit.to_owned(),
            }),
            Recovery::MarkInterrupted => Err(refused(FROM, event)),
        },
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::TaskCancelled { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::GateAcknowledged { .. } => Err(refused(FROM, event)),
    }
}

/// What an event means to a run that is standing still for a reason.
///
/// The pause holds where to go back to, so it is the one state whose events are
/// answered by something other than the transition table: `Resumed` and
/// recovery's `Resume` both hand back exactly what was put in, unchanged, which
/// is what makes a pause durable across the crash of a supervisor rather than a
/// guess. A second `Paused` is refused rather than nested: the state already
/// holds where the run resumes, so a second wait would either overwrite that
/// answer or ask the run to resume twice for one stop — ADR-0026. The reason a
/// pause is asked to hold does not narrow the refusal: a limit, a signal and a
/// blocked predecessor are all refused the same way, and the reason already
/// recorded is the one recovery acts on.
///
/// `reason` is consulted once, for the one event that depends on it: a gate is
/// acknowledged by a person, so only a pause that stopped *at* a gate may be
/// closed by an acknowledgement (VISION.md §6). A pause is never a failure
/// either: `TaskFailed` describes work that will not get done, and a paused run
/// has not been asked whether it can.
///
/// An attempt's record is refused here for the same reason `AttemptStarted` is:
/// a pause is a wait, not a run, so there is nothing in flight for evidence to
/// be about. The attempt a pause resumes into is the one that files its own
/// record, which is why filing is done while an attempt is still held rather
/// than whenever a run happens to get round to it.
fn from_paused(
    reason: &PauseReason,
    resume_to: &TaskState,
    event: &EventKind,
) -> Result<TaskState> {
    const FROM: &str = "Paused";
    let waiting = TaskState::Paused {
        reason: reason.clone(),
        resume_to: Box::new(resume_to.clone()),
    };
    match event {
        EventKind::Resumed => Ok(TaskState::clone(resume_to)),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::GateAcknowledged { by, at } => refuse_unless(
            matches!(reason, PauseReason::HumanGate),
            TaskState::Acknowledged {
                by: by.clone(),
                at: *at,
            },
            FROM,
            event,
        ),
        EventKind::RecoveryDecision { decision, .. } => match decision {
            Recovery::Resume => Ok(TaskState::clone(resume_to)),
            Recovery::AlreadyApplied => Ok(waiting),
            Recovery::MarkInterrupted => Err(refused(FROM, event)),
        },
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Paused { .. }
        | EventKind::Interrupted { .. } => Err(refused(FROM, event)),
    }
}

/// The states a run is busy with: started, and neither stopped nor finished.
///
/// Six of the twelve — everything past [`TaskState::Queued`] that ADR-0021 does
/// not call terminal and ADR-0026 does not call paused. Each of the six has
/// work in flight on one machine: `Preflight` is running the checks that gate
/// the tree, [`TaskState::Running`] and [`TaskState::Remediating`] have an agent
/// in front of them, [`TaskState::Verifying`] is spending the gates on one
/// attempt's output, [`TaskState::Publishing`] has a push in the air, and
/// [`TaskState::PublishedVerified`] is a commit the remote holds that the queue
/// has not closed. Two tasks in any two of them contend for the same tree and
/// the same remote, which is what VISION.md §3 invariant 1 forbids.
///
/// [`TaskState::Paused`] is off the list on purpose: a pause is how a run gives
/// the machine back, and whether a paused task also blocks its successor is
/// `next_runnable`'s rule to state (T032), not this predicate's. The match is
/// exhaustive rather than a `matches!` so a thirteenth state is placed on one
/// side of the line by the compiler instead of defaulted onto it — the reasoning
/// ADR-0022 applies to [`apply`].
fn is_active(state: &TaskState) -> bool {
    match state {
        TaskState::Preflight
        | TaskState::Running { .. }
        | TaskState::Remediating { .. }
        | TaskState::Verifying { .. }
        | TaskState::Publishing { .. }
        | TaskState::PublishedVerified { .. } => true,
        TaskState::Queued
        | TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Paused { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled => false,
    }
}

/// VISION.md §3 invariant 1 made checkable: the queue cannot have two tasks
/// running.
///
/// `states` is the projection of every task the queue holds — the same map a
/// rebuild folds and a screen prints — so the question is asked of the whole
/// queue and not only of the tasks that could still be started. One active task
/// is legal, and so is none: a freshly imported plan is rows of
/// [`TaskState::Queued`] with nothing running, and a queue stopped at a pause or
/// a failure holds tasks that have stopped rather than tasks that are running.
///
/// The check reads and decides nothing else — no clock, no journal, no lock —
/// which is what lets the same call answer before a task is started, after a
/// transition has been journaled, and over a replay that recovered a crash.
///
/// # Errors
///
/// [`Error::Policy`] when more than one task is active, naming every one of them
/// — `task 2 (Running), task 7 (Verifying)` — in id order rather than only the
/// first two: with three active tasks the third is the one whose worktree the
/// next step opens, and a reader should not have to run the check again to find
/// out it exists. No path is listed, because a row of the queue broke the rule
/// rather than a file.
pub fn check_one_active(states: &BTreeMap<TaskId, TaskState>) -> Result<()> {
    let active = states
        .iter()
        .filter(|(_, state)| is_active(state))
        .collect::<Vec<_>>();
    if active.len() > 1 {
        return Err(Error::Policy {
            detail: format!(
                "{} tasks are active at once ({}); exactly one is allowed, and no \
                 further task may start until the rest have finished, paused, or \
                 been cancelled",
                active.len(),
                active
                    .iter()
                    .map(|(id, state)| format!("task {} ({})", id, state.name()))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            paths: Vec::new(),
        });
    }
    Ok(())
}

/// Whether a task in `state` is work the queue may proceed past.
///
/// Four of the twelve states: [`TaskState::PublishedVerified`], whose commit
/// the remote was read back holding, [`TaskState::Done`], which closes over it,
/// [`TaskState::Acknowledged`], which closes a gate that could never publish
/// anything, and [`TaskState::Cancelled`], which a human dropped. The match is exhaustive
/// rather than a `matches!` for the reason ADR-0022 applies to [`apply`]: a
/// thirteenth state has to be placed on one side of the line by the compiler
/// instead of defaulted onto the safe-looking side. A gate produces no commit, so
/// `Acknowledged` is the only state a gate can finish in — leaving it out would
/// hold everything below one gate waiting for a publication that can never
/// happen (VISION.md §6, ADR-0031).
fn clears_the_way(state: &TaskState) -> bool {
    match state {
        TaskState::PublishedVerified { .. }
        | TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Cancelled => true,
        TaskState::Queued
        | TaskState::Preflight
        | TaskState::Running { .. }
        | TaskState::Remediating { .. }
        | TaskState::Verifying { .. }
        | TaskState::Publishing { .. }
        | TaskState::Paused { .. }
        | TaskState::Failed { .. } => false,
    }
}

/// VISION.md §3 invariant 2 made checkable: a successor cannot start before its
/// predecessor is verified published.
///
/// `states` is the projection of every task the queue holds and `next` is the id
/// about to be started, so the question is the same one [`check_one_active`] is
/// asked of: pure, no clock, no journal, no lock, answering identically before a
/// task starts, after a transition has been journaled, and over a replay that
/// recovered a crash. Order is the queue's own — the ids are its 1-based
/// positions — so "predecessor" means every id strictly lower than `next`, and
/// nothing else: `next`'s own row and the rows after it are not its
/// predecessors, which is what lets a freshly imported plan of nothing but
/// [`TaskState::Queued`] rows have a head at all.
///
/// A predecessor clears the way in exactly the four states `clears_the_way`
/// names — published, closed, acknowledged by a human, or cancelled — so work
/// still in flight, work that failed, and work standing still in a
/// [`TaskState::Paused`] all hold the successor where it is. A gate clears the way
/// on the acknowledgement alone and never on a commit: it is not an executable
/// task, so an unpassed gate holds what is below it and a passed one lets it go
/// (VISION.md §3 invariant 2). `next` does not have to be a row of
/// `states`: the answer is about what lies below the id, which is what lets a
/// candidate id be asked about before its row exists.
///
/// # Errors
///
/// [`Error::Policy`] when a lower id has not been published, naming every one of
/// them — `task 2 (Failed), task 6 (Paused)` — in id order rather than only
/// the first: with three unpublished predecessors the one named is the one whose
/// publication the next step is waiting on, and a reader should not have to run
/// the check again to find the others. The refusal opens with `next`, because the
/// task being refused to start is the fact a human or a screen acts on. No path
/// is listed, because a row of the queue broke the rule rather than a file.
pub fn check_predecessor(states: &BTreeMap<TaskId, TaskState>, next: TaskId) -> Result<()> {
    let unpublished = states
        .iter()
        .filter(|(id, _)| **id < next)
        .filter(|(_, state)| !clears_the_way(state))
        .map(|(id, state)| format!("task {} ({})", id, state.name()))
        .collect::<Vec<_>>();
    if !unpublished.is_empty() {
        return Err(Error::Policy {
            detail: format!(
                "task {next} cannot start: its predecessors ({}) are neither Done, \
                 Acknowledged, Cancelled nor PublishedVerified, and a successor may \
                 not start until every lower id is",
                unpublished.join(", ")
            ),
            paths: Vec::new(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        PauseReason, Phase, PhaseEntry, Recovery, Stream, TaskState, apply, check_one_active,
        check_predecessor, phase_entry,
    };
    use crate::{AttemptId, AttemptRecord, Error, EventKind, FailureClass, TaskId};
    use serde::de::DeserializeOwned;
    use std::collections::BTreeMap;
    use std::fmt::Debug;

    /// Every `Phase`, in the order `docs/DESIGN.md` declares them.
    const PHASES: [Phase; 12] = [
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
    ];

    /// The same twelve names as `docs/DESIGN.md` spells them, written out a
    /// second time so a rename cannot slip through by matching itself.
    const PHASE_NAMES: [&str; 12] = [
        "Goal",
        "Scope",
        "AcceptanceTests",
        "Implement",
        "Red",
        "Green",
        "Refactor",
        "Review",
        "Harden",
        "DoneCheck",
        "Verify",
        "Publish",
    ];

    /// Every `PauseReason`. `Limit` is listed with no reset time because the
    /// instant it may carry is not part of the variant's identity.
    const PAUSE_REASONS: [PauseReason; 5] = [
        PauseReason::Limit { until: None },
        PauseReason::Input,
        PauseReason::HumanGate,
        PauseReason::Interrupted,
        PauseReason::Blocked,
    ];

    /// The four `PauseReason`s that carry nothing, by their documented names.
    const PAUSE_REASON_NAMES: [&str; 4] = ["Input", "HumanGate", "Interrupted", "Blocked"];

    /// Encodes `value`, insists it carries the documented name, reads it back.
    fn round_trips<T>(value: &T, name: &str)
    where
        T: Clone + serde::Serialize + DeserializeOwned + PartialEq + Debug,
    {
        let encoded = serde_json::to_string(value).expect("vocabulary encodes as JSON");
        assert_eq!(
            encoded,
            format!("\"{name}\""),
            "{name} must encode as its own name"
        );
        let decoded: T =
            serde_json::from_str(&encoded).expect("a documented encoding is read back");
        assert_eq!(&decoded, value, "{name} must survive the round trip");
    }

    #[test]
    fn phase_has_the_twelve_variants_docs_design_md_names() {
        assert_eq!(PHASES.len(), 12);
        assert_eq!(PHASE_NAMES.len(), PHASES.len());
        for (phase, name) in PHASES.iter().zip(PHASE_NAMES) {
            round_trips(phase, name);
        }
    }

    #[test]
    fn pause_reason_has_the_five_variants_docs_design_md_names() {
        assert_eq!(PAUSE_REASONS.len(), 5);
        assert_eq!(PAUSE_REASON_NAMES.len(), PAUSE_REASONS.len() - 1);
        for (reason, name) in PAUSE_REASONS[1..].iter().zip(PAUSE_REASON_NAMES) {
            round_trips(reason, name);
        }

        let encoded = serde_json::to_string(&PauseReason::Limit { until: None })
            .expect("a limit pause encodes as JSON");
        assert_eq!(encoded, r#"{"Limit":{"until":null}}"#);
        assert_eq!(
            serde_json::from_str::<PauseReason>(&encoded).expect("the encoding is read back"),
            PauseReason::Limit { until: None },
        );
    }

    #[test]
    fn a_paused_run_keeps_the_instant_it_waits_until_through_json() {
        let until = time::macros::datetime!(2026-09-17 12:34:56 UTC);
        let paused = PauseReason::Limit { until: Some(until) };
        let encoded = serde_json::to_string(&paused).expect("a reset time encodes as JSON");
        match serde_json::from_str::<PauseReason>(&encoded).expect("the encoding is read back") {
            PauseReason::Limit { until: decoded } => assert_eq!(decoded, Some(until)),
            other => panic!("a limit pause must come back as a limit pause, got {other:?}"),
        }
    }

    #[test]
    fn stream_has_the_two_variants_docs_design_md_names() {
        const STREAMS: [Stream; 2] = [Stream::Stdout, Stream::Stderr];
        const NAMES: [&str; 2] = ["Stdout", "Stderr"];
        assert_eq!(STREAMS.len(), 2);
        for (stream, name) in STREAMS.iter().zip(NAMES) {
            round_trips(stream, name);
        }
    }

    #[test]
    fn recovery_has_the_three_variants_docs_design_md_names() {
        const RECOVERIES: [Recovery; 3] = [
            Recovery::Resume,
            Recovery::MarkInterrupted,
            Recovery::AlreadyApplied,
        ];
        const NAMES: [&str; 3] = ["Resume", "MarkInterrupted", "AlreadyApplied"];
        assert_eq!(RECOVERIES.len(), 3);
        for (recovery, name) in RECOVERIES.iter().zip(NAMES) {
            round_trips(recovery, name);
        }
    }

    #[test]
    fn the_vocabulary_refuses_a_name_no_document_names() {
        for rejected in ["SpecFirst", "implement", "Done", ""] {
            assert!(
                serde_json::from_str::<Phase>(&format!("\"{rejected}\"")).is_err(),
                "{rejected} is not a phase and must not deserialise as one",
            );
        }
        assert!(serde_json::from_str::<PauseReason>("\"Limit\"").is_err());
        assert!(serde_json::from_str::<Stream>("\"Output\"").is_err());
        assert!(serde_json::from_str::<Recovery>("\"Restart\"").is_err());
    }

    /// One instance of every `TaskState`, in the order `docs/DESIGN.md`
    /// declares them. Written out by hand rather than generated, because the
    /// point of the list is that a person named each entry.
    fn one_state_per_variant() -> [TaskState; 12] {
        [
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            TaskState::PublishedVerified {
                commit: "b7d1f3a".to_owned(),
            },
            TaskState::Done,
            TaskState::Acknowledged {
                by: "illya".to_owned(),
                at: time::macros::datetime!(2026-09-17 12:34:56 UTC),
            },
            TaskState::Paused {
                reason: PauseReason::HumanGate,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Green,
                }),
            },
            TaskState::Failed {
                class: FailureClass::ProviderLimit,
                detail: "429 from the provider".to_owned(),
            },
            TaskState::Cancelled,
        ]
    }

    /// The same twelve names as `docs/DESIGN.md` spells them, written out a
    /// second time so a rename cannot slip through by matching itself.
    const TASK_STATE_NAMES: [&str; 12] = [
        "Queued",
        "Preflight",
        "Running",
        "Remediating",
        "Verifying",
        "Publishing",
        "PublishedVerified",
        "Done",
        "Acknowledged",
        "Paused",
        "Failed",
        "Cancelled",
    ];

    /// Which of the twelve a supervisor does no further work on, in the same
    /// order as [`one_state_per_variant`].
    const TERMINAL: [bool; 12] = [
        false, false, false, false, false, false, false, true, true, false, true, true,
    ];

    /// Which of the twelve is standing still rather than finished, likewise.
    const PAUSED: [bool; 12] = [
        false, false, false, false, false, false, false, false, false, true, false, false,
    ];

    /// Encodes `state` and reads the encoding straight back: the journal stores
    /// exactly this text, so a state that does not survive this is a state that
    /// cannot be recovered from a crash.
    fn state_through_json(state: &TaskState) -> TaskState {
        let encoded = serde_json::to_string(state).expect("a task state encodes as JSON");
        serde_json::from_str(&encoded).expect("a documented state encoding is read back")
    }

    #[test]
    fn task_state_has_the_twelve_variants_docs_design_md_names() {
        let states = one_state_per_variant();
        assert_eq!(states.len(), 12);
        assert_eq!(TASK_STATE_NAMES.len(), states.len());
        for (state, name) in states.iter().zip(TASK_STATE_NAMES) {
            assert_eq!(
                state.name(),
                name,
                "name() must report the documented variant"
            );
            let encoded = serde_json::to_string(state).expect("a task state encodes as JSON");
            assert!(
                encoded == format!("\"{name}\"") || encoded.starts_with(&format!("{{\"{name}\":")),
                "{name} must be tagged with the name the document gives it, got {encoded}"
            );
            assert_eq!(
                state_through_json(state),
                *state,
                "{name} must survive the JSON round trip"
            );
        }
    }

    #[test]
    fn a_state_payload_encodes_the_fields_docs_design_md_lists() {
        let running = TaskState::Running {
            attempt: AttemptId::new(2),
            phase: Phase::Implement,
        };
        assert_eq!(
            serde_json::to_string(&running).expect("a running state encodes as JSON"),
            r#"{"Running":{"attempt":2,"phase":"Implement"}}"#
        );
        let remediating = TaskState::Remediating {
            attempt: AttemptId::new(3),
            phase: Phase::Refactor,
        };
        assert_eq!(
            serde_json::to_string(&remediating).expect("a remediation encodes as JSON"),
            r#"{"Remediating":{"attempt":3,"phase":"Refactor"}}"#
        );
        let published = TaskState::PublishedVerified {
            commit: "b7d1f3a".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&published).expect("a published commit encodes as JSON"),
            r#"{"PublishedVerified":{"commit":"b7d1f3a"}}"#
        );
        let failed = TaskState::Failed {
            class: FailureClass::ProviderLimit,
            detail: "429 from the provider".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&failed).expect("a failure encodes as JSON"),
            r#"{"Failed":{"class":"ProviderLimit","detail":"429 from the provider"}}"#
        );
        let at = time::macros::datetime!(2026-09-17 12:34:56.123456789 UTC);
        let acknowledged = TaskState::Acknowledged {
            by: "illya".to_owned(),
            at,
        };
        let encoded =
            serde_json::to_string(&acknowledged).expect("an acknowledgement encodes as JSON");
        assert!(
            encoded.starts_with(r#"{"Acknowledged":{"#),
            "an acknowledgement is tagged with its variant, got {encoded}"
        );
        assert!(
            encoded.contains(r#""by":"illya""#),
            "who acknowledged is kept beside the instant, got {encoded}"
        );
        assert!(
            encoded.contains(r#""at":"#),
            "when it was acknowledged is kept, got {encoded}"
        );
        let decoded = state_through_json(&acknowledged);
        let TaskState::Acknowledged { at: kept, .. } = decoded else {
            panic!("an acknowledgement must come back as one, got {decoded:?}");
        };
        assert_eq!(
            kept, at,
            "an acknowledgement instant must survive to the nanosecond"
        );

        let stamped: serde_json::Value =
            serde_json::from_str(&encoded).expect("an acknowledgement is JSON");
        let vocabulary: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&PauseReason::Limit { until: Some(at) })
                .expect("a pause reason encodes as JSON"),
        )
        .expect("a pause reason is JSON");
        assert_eq!(
            stamped
                .get("Acknowledged")
                .and_then(|field| field.get("at")),
            vocabulary.get("Limit").and_then(|field| field.get("until")),
            "a state must timestamp an instant the way this vocabulary already does, \
             not invent a form of its own"
        );
    }

    #[test]
    fn a_pause_carries_the_state_it_resumes_to_inside_itself() {
        let paused = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Queued),
        };
        assert_eq!(
            serde_json::to_string(&paused).expect("a paused state encodes as JSON"),
            r#"{"Paused":{"reason":"Input","resume_to":"Queued"}}"#,
            "the state to resume into belongs inside the pause, not beside it"
        );
    }

    /// The codec's claim, not the machine's: `apply` refuses to build a pause
    /// above a pause (ADR-0026), but the encoding is durable data, so a journal
    /// that already holds one has to read back rather than fail to open.
    #[test]
    fn a_pause_nested_in_a_pause_keeps_its_own_resume_state_and_instant() {
        let until = time::macros::datetime!(2026-09-17 12:34:56 UTC);
        let nested = TaskState::Paused {
            reason: PauseReason::Limit { until: Some(until) },
            resume_to: Box::new(TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(2),
                    phase: Phase::Red,
                }),
            }),
        };
        let decoded = state_through_json(&nested);
        assert_eq!(decoded, nested, "two levels of pause must both come back");
        let TaskState::Paused { reason, resume_to } = &decoded else {
            panic!("a paused state must return as a pause, got {decoded:?}");
        };
        assert_eq!(reason, &PauseReason::Limit { until: Some(until) });
        let TaskState::Paused {
            reason: inner_reason,
            resume_to: inner,
        } = resume_to.as_ref()
        else {
            panic!("the inner pause must return as a pause, got {resume_to:?}");
        };
        assert_eq!(
            inner_reason,
            &PauseReason::Interrupted,
            "the inner pause must keep its own reason, not the outer one"
        );
        assert_eq!(
            **inner,
            TaskState::Running {
                attempt: AttemptId::new(2),
                phase: Phase::Red
            },
            "the phase a nested pause resumes into must be the one that was stored"
        );
    }

    #[test]
    fn terminal_states_are_the_four_a_supervisor_does_no_further_work_on() {
        let states = one_state_per_variant();
        for ((state, name), terminal) in states.iter().zip(TASK_STATE_NAMES).zip(TERMINAL) {
            assert_eq!(
                state.is_terminal(),
                terminal,
                "{name} disagrees about being terminal"
            );
        }
    }

    #[test]
    fn only_a_paused_state_reports_itself_paused() {
        let states = one_state_per_variant();
        for ((state, name), paused) in states.iter().zip(TASK_STATE_NAMES).zip(PAUSED) {
            assert_eq!(
                state.is_paused(),
                paused,
                "{name} disagrees about being paused"
            );
        }
    }

    #[test]
    fn a_pause_is_neither_terminal_nor_finished_by_what_it_would_resume_into() {
        let over_done = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Done),
        };
        assert!(
            over_done.is_paused(),
            "a pause still stands still, whatever it would resume into"
        );
        assert!(
            !over_done.is_terminal(),
            "a pause still owes a resume, even above a finished state"
        );
        let TaskState::Paused { resume_to, .. } = &over_done else {
            panic!("the state built above is a pause");
        };
        assert!(
            resume_to.is_terminal(),
            "the boxed state is reported on its own merits"
        );
        assert!(!resume_to.is_paused());
    }

    #[test]
    fn a_state_encoding_refuses_what_docs_design_md_does_not_define() {
        for rejected in [
            "\"Waiting\"",
            "\"running\"",
            r#"{"Running":{"attempt":2}}"#,
            r#"{"Running":{"attempt":2,"phase":"SpecFirst"}}"#,
            r#"{"Paused":{"reason":"Input"}}"#,
            r#"{"Failed":{"class":"ProviderLimit"}}"#,
        ] {
            assert!(
                serde_json::from_str::<TaskState>(rejected).is_err(),
                "{rejected} is not a documented state encoding and must not be read as one"
            );
        }
    }

    /// The title a task carries into the queue.
    const TITLE: &str = "The transition function";
    /// The commit a publication is proved by: the fetched remote holds it.
    const CANDIDATE: &str = "b7d1f3a";
    /// A commit nothing ever proved the remote holds. A test needs a commit
    /// real enough to name in an event and unproved enough that no state may
    /// carry it, to ask what happens when the two disagree.
    const UNPROVED: &str = "0000000";
    /// The commit preflight found, which every later commit is checked against.
    const BASE: &str = "0a1b2c3";

    /// The `attempt`th attempt working on `phase`.
    fn working(attempt: u32, phase: Phase) -> TaskState {
        TaskState::Running {
            attempt: AttemptId::new(attempt),
            phase,
        }
    }

    /// The `attempt`th attempt remediating a failure, on `phase`.
    fn remediating(attempt: u32, phase: Phase) -> TaskState {
        TaskState::Remediating {
            attempt: AttemptId::new(attempt),
            phase,
        }
    }

    /// The gates deciding whether the `attempt`th attempt counts.
    fn verifying(attempt: u32) -> TaskState {
        TaskState::Verifying {
            attempt: AttemptId::new(attempt),
        }
    }

    /// The `attempt`th attempt being committed and pushed.
    fn publishing(attempt: u32) -> TaskState {
        TaskState::Publishing {
            attempt: AttemptId::new(attempt),
        }
    }

    /// Work whose commit the remote was read back holding.
    fn published(commit: &str) -> TaskState {
        TaskState::PublishedVerified {
            commit: commit.to_owned(),
        }
    }

    /// `state`, parked for `reason`, holding itself as where to come back to.
    fn parked(state: TaskState, reason: PauseReason) -> TaskState {
        TaskState::Paused {
            reason,
            resume_to: Box::new(state),
        }
    }

    /// The acknowledged gate, closed by the same hand that opened the event.
    fn acknowledged_state() -> TaskState {
        TaskState::Acknowledged {
            by: "illya".to_owned(),
            at: time::macros::datetime!(2026-09-17 12:34:56 UTC),
        }
    }

    /// Every state a pause can hold: each that is neither finished nor already
    /// standing still.
    fn resumable_states() -> Vec<TaskState> {
        one_state_per_variant()
            .into_iter()
            .filter(|state| !state.is_terminal() && !state.is_paused())
            .collect()
    }

    fn queued() -> EventKind {
        EventKind::TaskQueued {
            title: TITLE.to_owned(),
        }
    }

    fn preflight_passed() -> EventKind {
        EventKind::PreflightPassed {
            base_sha: BASE.to_owned(),
        }
    }

    fn preflight_failed() -> EventKind {
        EventKind::PreflightFailed {
            class: FailureClass::EnvironmentFailure,
            detail: "no remote is configured".to_owned(),
        }
    }

    fn started(attempt: u32) -> EventKind {
        EventKind::AttemptStarted {
            attempt: AttemptId::new(attempt),
            protocol: "direct".to_owned(),
            pid: 4_321,
            base_sha: BASE.to_owned(),
        }
    }

    fn entered(attempt: u32, phase: Phase) -> EventKind {
        EventKind::PhaseEntered {
            attempt: AttemptId::new(attempt),
            phase,
        }
    }

    fn output(attempt: u32) -> EventKind {
        EventKind::AgentOutput {
            attempt: AttemptId::new(attempt),
            stream: Stream::Stderr,
            text: "reading the plan".to_owned(),
        }
    }

    fn verify_passed(attempt: u32) -> EventKind {
        EventKind::VerifyPassed {
            attempt: AttemptId::new(attempt),
        }
    }

    fn verify_failed(attempt: u32) -> EventKind {
        EventKind::VerifyFailed {
            attempt: AttemptId::new(attempt),
            class: FailureClass::VerificationFailure,
            detail: "one gate failed".to_owned(),
        }
    }

    fn publish_started(attempt: u32) -> EventKind {
        EventKind::PublishStarted {
            attempt: AttemptId::new(attempt),
            candidate_sha: CANDIDATE.to_owned(),
        }
    }

    fn publish_verified(commit: &str, remote_sha: &str) -> EventKind {
        EventKind::PublishVerified {
            commit: commit.to_owned(),
            remote_sha: remote_sha.to_owned(),
        }
    }

    fn task_done(commit: &str) -> EventKind {
        EventKind::TaskDone {
            commit: commit.to_owned(),
        }
    }

    fn task_failed() -> EventKind {
        EventKind::TaskFailed {
            class: FailureClass::AgentFailure,
            detail: "the agent gave up".to_owned(),
        }
    }

    /// The state `task_failed` leaves behind.
    fn failure() -> TaskState {
        TaskState::Failed {
            class: FailureClass::AgentFailure,
            detail: "the agent gave up".to_owned(),
        }
    }

    fn cancelled() -> EventKind {
        EventKind::TaskCancelled {
            reason: "superseded by a later plan".to_owned(),
        }
    }

    fn pause(reason: PauseReason) -> EventKind {
        EventKind::Paused { reason }
    }

    fn interrupted(phase: Phase) -> EventKind {
        EventKind::Interrupted { phase }
    }

    fn recovered(decision: Recovery) -> EventKind {
        EventKind::RecoveryDecision {
            decision,
            detail: "the journal ends mid-phase".to_owned(),
        }
    }

    fn acknowledged() -> EventKind {
        EventKind::GateAcknowledged {
            by: "illya".to_owned(),
            at: time::macros::datetime!(2026-09-17 12:34:56 UTC),
        }
    }

    /// An attempt's evidence, naming the attempt `attempt`. It carries no
    /// verdict, so nothing here can mistake it for a transition.
    fn attempt_recorded(attempt: u32) -> EventKind {
        EventKind::AttemptRecorded {
            record: Box::new(AttemptRecord {
                id: AttemptId::new(attempt),
                task: TaskId::new(1),
                started: time::macros::datetime!(2026-09-17 12:00:00 UTC),
                ended: Some(time::macros::datetime!(2026-09-17 12:20:00 UTC)),
                model_configured: Some("gpt-5.6-sol".to_owned()),
                model_reported: None,
                session_id: Some("sess_01HQZK".to_owned()),
                exit_reason: "exited 0".to_owned(),
                gates: Vec::new(),
                usage: None,
                base_sha: BASE.to_owned(),
                candidate_sha: Some(CANDIDATE.to_owned()),
            }),
        }
    }

    /// Every catalog entry a journal can hold, carrying what a run would carry.
    /// Written out by hand rather than generated because the point of the list
    /// is that a person named each entry — and because a sweep over it is what
    /// proves no state stays quiet about an event.
    fn every_event() -> [EventKind; 20] {
        [
            queued(),
            EventKind::PreflightStarted,
            preflight_passed(),
            preflight_failed(),
            started(1),
            entered(1, Phase::Implement),
            output(1),
            verify_passed(1),
            verify_failed(1),
            publish_started(1),
            publish_verified(CANDIDATE, CANDIDATE),
            task_done(CANDIDATE),
            task_failed(),
            cancelled(),
            pause(PauseReason::Input),
            EventKind::Resumed,
            interrupted(Phase::Implement),
            recovered(Recovery::Resume),
            acknowledged(),
            attempt_recorded(1),
        ]
    }

    /// Applies `event` to `state` and insists it produced exactly `expected`.
    fn moves(state: &TaskState, event: &EventKind, expected: &TaskState) {
        match apply(state, event) {
            Ok(moved) => assert_eq!(
                &moved,
                expected,
                "{} moved to {} instead of {} on {}",
                state.name(),
                moved.name(),
                expected.name(),
                event.discriminant()
            ),
            Err(error) => panic!(
                "{} must accept {}, got {error:?}",
                state.name(),
                event.discriminant()
            ),
        }
    }

    /// Applies `event` to `state` and insists the refusal named both of them.
    fn refuses(state: &TaskState, event: &EventKind) {
        match apply(state, event) {
            Err(Error::InvalidTransition {
                from,
                event: refused,
            }) => {
                assert_eq!(
                    from,
                    state.name(),
                    "the refusal must name the state that refused"
                );
                assert_eq!(
                    refused,
                    event.discriminant(),
                    "the refusal must name the event that was refused"
                );
            }
            Err(other) => panic!(
                "{} must refuse {} as an illegal transition, got {other:?}",
                state.name(),
                event.discriminant()
            ),
            Ok(moved) => panic!(
                "{} must refuse {}, moved to {}",
                state.name(),
                event.discriminant(),
                moved.name()
            ),
        }
    }

    #[test]
    fn the_happy_path_walks_the_pipeline_from_queued_to_done() {
        let steps = [
            (queued(), TaskState::Queued),
            (EventKind::PreflightStarted, TaskState::Preflight),
            (preflight_passed(), TaskState::Preflight),
            (started(1), TaskState::Preflight),
            (entered(1, Phase::Implement), working(1, Phase::Implement)),
            (output(1), working(1, Phase::Implement)),
            (entered(1, Phase::Verify), verifying(1)),
            (verify_passed(1), publishing(1)),
            (publish_started(1), publishing(1)),
            (publish_verified(CANDIDATE, CANDIDATE), published(CANDIDATE)),
            (task_done(CANDIDATE), TaskState::Done),
        ];

        let mut state = TaskState::Queued;
        for (event, expected) in &steps {
            moves(&state, event, &expected.clone());
            state = expected.clone();
        }
        assert_eq!(state, TaskState::Done, "the path must end where Done is");
    }

    #[test]
    fn a_failed_gate_remediates_in_a_later_attempt_and_still_finishes() {
        let steps = [
            (verify_failed(1), working(1, Phase::Green)),
            (started(2), working(1, Phase::Green)),
            (entered(2, Phase::Red), remediating(2, Phase::Red)),
            (output(2), remediating(2, Phase::Red)),
            (entered(2, Phase::Verify), verifying(2)),
            (verify_passed(2), publishing(2)),
            (publish_started(2), publishing(2)),
            (publish_verified(CANDIDATE, CANDIDATE), published(CANDIDATE)),
            (task_done(CANDIDATE), TaskState::Done),
        ];

        let mut state = working(1, Phase::Green);
        for (event, expected) in &steps {
            moves(&state, event, &expected.clone());
            state = expected.clone();
        }
        assert_eq!(
            state,
            TaskState::Done,
            "a remediated task ends the same way"
        );
    }

    /// The ten phases an agent works, which are every phase but the two the
    /// machine itself owns.
    fn work_phases() -> [Phase; 10] {
        [
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
        ]
    }

    #[test]
    fn a_phase_entry_says_which_half_of_the_machine_the_phase_belongs_to() {
        assert_eq!(phase_entry(Phase::Verify), PhaseEntry::Gates);
        assert_eq!(phase_entry(Phase::Publish), PhaseEntry::Publication);
        for phase in work_phases() {
            assert_eq!(
                phase_entry(phase),
                PhaseEntry::Work,
                "{phase:?} is an agent's"
            );
        }
        assert_eq!(work_phases().len() + 2, PHASES.len());
    }

    #[test]
    fn a_queued_task_is_the_seed_its_own_event_replays_onto() {
        moves(&TaskState::Queued, &queued(), &TaskState::Queued);
    }

    #[test]
    fn a_queued_task_starts_preflight_and_nothing_else() {
        moves(
            &TaskState::Queued,
            &EventKind::PreflightStarted,
            &TaskState::Preflight,
        );
        for event in [
            preflight_passed(),
            preflight_failed(),
            started(1),
            entered(1, Phase::Implement),
            output(1),
            verify_passed(1),
            verify_failed(1),
            publish_started(1),
            publish_verified(CANDIDATE, CANDIDATE),
            task_done(CANDIDATE),
            EventKind::Resumed,
            interrupted(Phase::Implement),
        ] {
            refuses(&TaskState::Queued, &event);
        }
    }

    #[test]
    fn a_queued_task_waits_or_is_cancelled_but_never_fails() {
        moves(
            &TaskState::Queued,
            &pause(PauseReason::Blocked),
            &parked(TaskState::Queued, PauseReason::Blocked),
        );
        moves(&TaskState::Queued, &cancelled(), &TaskState::Cancelled);
        refuses(&TaskState::Queued, &task_failed());
    }

    #[test]
    fn preflight_records_the_checks_that_leave_nothing_the_state_can_hold() {
        for event in [
            EventKind::PreflightStarted,
            preflight_passed(),
            started(1),
            started(2),
        ] {
            moves(&TaskState::Preflight, &event, &TaskState::Preflight);
        }
    }

    #[test]
    fn a_preflight_refusal_fails_the_task_with_the_class_the_check_found() {
        let expected = TaskState::Failed {
            class: FailureClass::EnvironmentFailure,
            detail: "no remote is configured".to_owned(),
        };
        moves(&TaskState::Preflight, &preflight_failed(), &expected);
        refuses(&TaskState::Preflight, &task_failed());
    }

    #[test]
    fn preflight_refuses_the_evidence_no_attempt_has_produced_yet() {
        for event in [
            queued(),
            output(1),
            verify_passed(1),
            verify_failed(1),
            publish_started(1),
            publish_verified(CANDIDATE, CANDIDATE),
            task_done(CANDIDATE),
            task_failed(),
            acknowledged(),
            EventKind::Resumed,
            interrupted(Phase::Implement),
        ] {
            refuses(&TaskState::Preflight, &event);
        }
    }

    #[test]
    fn an_attempt_entering_a_phase_starts_the_work() {
        for phase in work_phases() {
            moves(
                &TaskState::Preflight,
                &entered(1, phase),
                &working(1, phase),
            );
        }
    }

    #[test]
    fn entering_the_verify_phase_hands_the_work_to_the_gates() {
        moves(
            &TaskState::Preflight,
            &entered(1, Phase::Verify),
            &verifying(1),
        );
        moves(
            &working(1, Phase::Implement),
            &entered(1, Phase::Verify),
            &verifying(1),
        );
        moves(
            &remediating(2, Phase::Red),
            &entered(2, Phase::Verify),
            &verifying(2),
        );
    }

    #[test]
    fn only_verification_passing_opens_publication() {
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            working(1, Phase::Implement),
            remediating(2, Phase::Red),
            verifying(1),
            published(CANDIDATE),
        ] {
            refuses(&state, &entered(1, Phase::Publish));
            refuses(&state, &publish_started(1));
        }
        moves(&verifying(1), &verify_passed(1), &publishing(1));
    }

    #[test]
    fn verification_passing_opens_publication_whatever_the_attempt_was_doing() {
        moves(&working(1, Phase::Green), &verify_passed(1), &publishing(1));
        moves(
            &remediating(2, Phase::Red),
            &verify_passed(2),
            &publishing(2),
        );
        moves(&verifying(1), &verify_passed(1), &publishing(1));
        moves(&publishing(1), &verify_passed(1), &publishing(1));
    }

    #[test]
    fn a_failed_gate_leaves_the_task_where_the_remediation_starts_from() {
        for state in [
            working(1, Phase::Green),
            remediating(2, Phase::Red),
            verifying(1),
        ] {
            let event = verify_failed(match state {
                TaskState::Remediating { attempt, .. } => attempt.get(),
                _ => 1,
            });
            moves(&state, &event, &state.clone());
        }
        refuses(&publishing(1), &verify_failed(1));
        refuses(&TaskState::Queued, &verify_failed(1));
    }

    #[test]
    fn evidence_names_the_attempt_it_belongs_to_or_is_refused() {
        moves(
            &working(1, Phase::Implement),
            &output(1),
            &working(1, Phase::Implement),
        );
        refuses(&working(1, Phase::Implement), &output(2));
        refuses(&verifying(1), &verify_passed(2));
        refuses(&verifying(1), &verify_failed(2));
        refuses(&publishing(1), &publish_started(2));
        refuses(&remediating(2, Phase::Red), &verify_passed(1));
    }

    /// An attempt's record is evidence, not a verdict: every state holding the
    /// attempt it names answers it with the state it was asked from. The walk at
    /// the end is the shape the outcome asks for — a first attempt files its
    /// record, fails, and the retry that follows files a record of its own
    /// rather than landing on top of the first one.
    #[test]
    fn an_attempt_record_says_what_was_and_moves_the_attempt_it_names() {
        moves(
            &working(1, Phase::Green),
            &attempt_recorded(1),
            &working(1, Phase::Green),
        );
        moves(
            &remediating(2, Phase::Red),
            &attempt_recorded(2),
            &remediating(2, Phase::Red),
        );
        moves(&verifying(1), &attempt_recorded(1), &verifying(1));
        moves(&publishing(1), &attempt_recorded(1), &publishing(1));

        let first_attempt = working(1, Phase::Green);
        moves(&first_attempt, &attempt_recorded(1), &first_attempt);
        moves(&first_attempt, &verify_failed(1), &first_attempt);
        moves(&first_attempt, &started(2), &first_attempt);
        moves(
            &first_attempt,
            &entered(2, Phase::Red),
            &remediating(2, Phase::Red),
        );
        moves(
            &remediating(2, Phase::Red),
            &attempt_recorded(2),
            &remediating(2, Phase::Red),
        );
    }

    /// The other half of the same rule. A record cannot be about an attempt the
    /// state is not holding, and a state that holds no attempt cannot be asked
    /// about one at all — including a pause, whose held attempt is the one the
    /// run resumes into rather than one that ever ran.
    #[test]
    fn an_attempt_record_is_refused_by_a_state_it_cannot_be_about() {
        refuses(&working(1, Phase::Implement), &attempt_recorded(2));
        refuses(&remediating(2, Phase::Red), &attempt_recorded(1));
        refuses(&verifying(1), &attempt_recorded(3));
        refuses(&publishing(1), &attempt_recorded(2));
        refuses(
            &parked(working(1, Phase::Green), PauseReason::Interrupted),
            &attempt_recorded(1),
        );
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            published(CANDIDATE),
        ] {
            refuses(&state, &attempt_recorded(1));
        }

        // An attempt's evidence has to be filed while the attempt is held: by the
        // time a task has failed, been cancelled or closed, the run that would
        // have written the record is the one this state says is over.
        for state in [failure(), TaskState::Cancelled, TaskState::Done] {
            refuses(&state, &attempt_recorded(1));
        }
    }

    #[test]
    fn a_later_attempt_starts_where_a_working_state_can_hold_it() {
        for state in [
            working(1, Phase::Green),
            remediating(2, Phase::Red),
            verifying(1),
            publishing(1),
        ] {
            let next = match state {
                TaskState::Running { attempt, .. }
                | TaskState::Remediating { attempt, .. }
                | TaskState::Verifying { attempt }
                | TaskState::Publishing { attempt } => attempt.get() + 1,
                _ => unreachable!("every state above carries an attempt"),
            };
            moves(&state, &started(next), &state.clone());
            refuses(&state, &started(next - 1));
        }
        for state in [
            TaskState::Queued,
            published(CANDIDATE),
            parked(working(1, Phase::Green), PauseReason::Interrupted),
        ] {
            refuses(&state, &started(2));
        }
    }

    #[test]
    fn a_later_attempt_always_enters_the_remediation_state() {
        moves(
            &working(1, Phase::Green),
            &entered(2, Phase::Red),
            &remediating(2, Phase::Red),
        );
        moves(
            &remediating(2, Phase::Red),
            &entered(3, Phase::Green),
            &remediating(3, Phase::Green),
        );
        moves(
            &verifying(1),
            &entered(2, Phase::Implement),
            &remediating(2, Phase::Implement),
        );
        moves(
            &remediating(2, Phase::Red),
            &entered(2, Phase::Green),
            &remediating(2, Phase::Green),
        );
    }

    #[test]
    fn an_earlier_attempt_never_resumes_the_work_a_later_one_is_doing() {
        refuses(&working(2, Phase::Green), &entered(1, Phase::Red));
        refuses(&remediating(2, Phase::Red), &entered(1, Phase::Green));
        refuses(&verifying(1), &entered(1, Phase::Implement));
        refuses(&publishing(1), &entered(1, Phase::Implement));
    }

    #[test]
    fn the_gates_re_open_for_the_attempt_they_hold_or_a_later_one() {
        moves(&verifying(1), &entered(1, Phase::Verify), &verifying(1));
        moves(&verifying(1), &entered(2, Phase::Verify), &verifying(2));
        refuses(&verifying(2), &entered(1, Phase::Verify));
    }

    #[test]
    fn a_later_attempt_can_take_a_publication_back_to_the_gates() {
        moves(&publishing(1), &entered(2, Phase::Verify), &verifying(2));
        refuses(&publishing(1), &entered(1, Phase::Verify));
        refuses(&publishing(1), &entered(2, Phase::Publish));
    }

    #[test]
    fn nothing_is_published_until_the_remote_is_read_back_holding_the_commit() {
        moves(&publishing(1), &publish_started(1), &publishing(1));
        moves(
            &publishing(1),
            &publish_verified(CANDIDATE, CANDIDATE),
            &published(CANDIDATE),
        );
        refuses(&publishing(1), &publish_verified(CANDIDATE, "0000000"));
        refuses(&verifying(1), &publish_verified(CANDIDATE, CANDIDATE));
        refuses(
            &working(1, Phase::Green),
            &publish_verified(CANDIDATE, CANDIDATE),
        );
    }

    #[test]
    fn a_read_back_that_agrees_with_the_recorded_commit_changes_nothing() {
        moves(
            &published(CANDIDATE),
            &publish_verified(CANDIDATE, CANDIDATE),
            &published(CANDIDATE),
        );
        refuses(
            &published(CANDIDATE),
            &publish_verified("0000000", "0000000"),
        );
        refuses(
            &published(CANDIDATE),
            &publish_verified(CANDIDATE, "0000000"),
        );
    }

    /// VISION.md §3 invariants 4 and 7, in the one place the machine decides them.
    ///
    /// `Done` is reached from exactly one state, by exactly one event, and only
    /// when that event names the commit the state itself proved the remote
    /// holds. The two refusals the invariant is named for come first: the state
    /// that has the gates but no publication, and the publication whose commit
    /// the closing misnames.
    ///
    /// Then both commits vary. An arm that compared the payload against
    /// whichever commit the suite happens to test with, rather than against the
    /// one the state carries, answers every pair a single-commit test tries;
    /// sweeping the state's commit against the event's is what shows which of
    /// the two the arm actually reads.
    ///
    /// A pause parked over a proved publication is asked about by name because
    /// it is the one state a closing is tempting to forward — the pause holds
    /// that publication as where to resume, so forwarding looks like doing the
    /// work a resume would do. It is refused: a paused run is waiting, and
    /// `Resumed` is the event that ends the waiting (ADR-0026).
    #[test]
    fn nothing_is_done_except_of_the_commit_the_remote_holds() {
        refuses(&verifying(1), &task_done(CANDIDATE));
        refuses(&published(CANDIDATE), &task_done(UNPROVED));
        moves(
            &published(CANDIDATE),
            &task_done(CANDIDATE),
            &TaskState::Done,
        );

        for proved in [CANDIDATE, UNPROVED] {
            for closed in [CANDIDATE, UNPROVED] {
                let state = published(proved);
                let event = task_done(closed);
                if proved == closed {
                    moves(&state, &event, &TaskState::Done);
                } else {
                    refuses(&state, &event);
                }
            }
        }

        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            working(1, Phase::Green),
            remediating(2, Phase::Red),
            verifying(1),
            publishing(1),
            parked(verifying(1), PauseReason::Interrupted),
            parked(published(CANDIDATE), PauseReason::Interrupted),
            parked(published(UNPROVED), PauseReason::HumanGate),
        ] {
            for commit in [CANDIDATE, UNPROVED] {
                refuses(&state, &task_done(commit));
            }
        }
    }

    #[test]
    fn published_work_is_neither_a_failure_nor_a_cancellation() {
        for event in [
            task_failed(),
            cancelled(),
            verify_passed(1),
            verify_failed(1),
            interrupted(Phase::Publish),
            EventKind::Resumed,
            acknowledged(),
        ] {
            refuses(&published(CANDIDATE), &event);
        }
        moves(
            &published(CANDIDATE),
            &pause(PauseReason::HumanGate),
            &parked(published(CANDIDATE), PauseReason::HumanGate),
        );
    }

    #[test]
    fn only_a_pause_at_a_human_gate_is_acknowledged() {
        moves(
            &parked(TaskState::Queued, PauseReason::HumanGate),
            &acknowledged(),
            &acknowledged_state(),
        );
        for reason in PAUSE_REASONS {
            if !matches!(reason, PauseReason::HumanGate) {
                refuses(&parked(TaskState::Queued, reason.clone()), &acknowledged());
            }
        }
        for state in [
            TaskState::Queued,
            working(1, Phase::Implement),
            remediating(2, Phase::Red),
            published(CANDIDATE),
        ] {
            refuses(&state, &acknowledged());
        }
    }

    #[test]
    fn a_pause_holds_the_state_it_resumes_to() {
        for state in resumable_states() {
            let event = pause(PauseReason::Limit { until: None });
            let expected = parked(state.clone(), PauseReason::Limit { until: None });
            moves(&state, &event, &expected);
        }
    }

    #[test]
    fn resuming_returns_exactly_the_state_the_pause_held() {
        for state in resumable_states() {
            let paused = parked(state.clone(), PauseReason::Interrupted);
            moves(&paused, &EventKind::Resumed, &state.clone());
            moves(&paused, &recovered(Recovery::Resume), &state);
        }
    }

    #[test]
    fn a_pause_above_a_pause_is_refused() {
        for state in resumable_states() {
            let waiting = parked(state, PauseReason::Interrupted);
            for reason in PAUSE_REASONS {
                refuses(&waiting, &pause(reason));
            }
        }
        // The refusal reads whether the state *is* a pause, not what it boxes,
        // so the two shapes `apply` can no longer build are refused the same
        // way: a pause above a pause, and a pause above a finished state.
        refuses(
            &parked(
                parked(working(1, Phase::Green), PauseReason::Input),
                PauseReason::Limit { until: None },
            ),
            &pause(PauseReason::Blocked),
        );
        refuses(
            &parked(TaskState::Done, PauseReason::Interrupted),
            &pause(PauseReason::Input),
        );
    }

    #[test]
    fn an_interruption_parks_the_states_with_a_phase_in_flight() {
        for state in [
            working(1, Phase::Implement),
            remediating(2, Phase::Red),
            verifying(1),
            publishing(1),
        ] {
            let phase = match state {
                TaskState::Running { phase, .. } | TaskState::Remediating { phase, .. } => phase,
                TaskState::Verifying { .. } => Phase::Verify,
                _ => Phase::Publish,
            };
            let event = interrupted(phase);
            moves(
                &state,
                &event,
                &parked(state.clone(), PauseReason::Interrupted),
            );
        }
    }

    #[test]
    fn an_interruption_naming_another_phase_is_not_the_one_that_happened() {
        refuses(&working(1, Phase::Implement), &interrupted(Phase::Green));
        refuses(&remediating(2, Phase::Red), &interrupted(Phase::Green));
    }

    #[test]
    fn an_interruption_needs_a_phase_to_interrupt() {
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            published(CANDIDATE),
            parked(working(1, Phase::Green), PauseReason::Input),
        ] {
            refuses(&state, &interrupted(Phase::Implement));
        }
    }

    #[test]
    fn failure_ends_the_states_where_work_was_in_flight() {
        for state in [
            working(1, Phase::Green),
            remediating(2, Phase::Red),
            verifying(1),
            publishing(1),
        ] {
            moves(&state, &task_failed(), &failure());
        }
    }

    #[test]
    fn a_pause_is_never_a_failure() {
        for reason in PAUSE_REASONS {
            refuses(
                &parked(working(1, Phase::Green), reason.clone()),
                &task_failed(),
            );
        }
    }

    #[test]
    fn a_cancelled_task_leaves_every_state_that_has_not_been_published() {
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            working(1, Phase::Green),
            remediating(2, Phase::Red),
            verifying(1),
            publishing(1),
            parked(verifying(1), PauseReason::Blocked),
        ] {
            moves(&state, &cancelled(), &TaskState::Cancelled);
        }
    }

    #[test]
    fn recovery_that_applies_nothing_changes_nothing() {
        for state in resumable_states() {
            moves(&state, &recovered(Recovery::AlreadyApplied), &state.clone());
        }
        let paused = parked(working(1, Phase::Green), PauseReason::Limit { until: None });
        moves(
            &paused,
            &recovered(Recovery::AlreadyApplied),
            &paused.clone(),
        );
    }

    #[test]
    fn recovery_marks_interrupted_only_where_a_phase_was_in_flight() {
        for state in [
            working(1, Phase::Green),
            remediating(2, Phase::Red),
            verifying(1),
            publishing(1),
        ] {
            let event = recovered(Recovery::MarkInterrupted);
            moves(
                &state,
                &event,
                &parked(state.clone(), PauseReason::Interrupted),
            );
        }
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            published(CANDIDATE),
            parked(working(1, Phase::Green), PauseReason::Input),
        ] {
            refuses(&state, &recovered(Recovery::MarkInterrupted));
        }
    }

    #[test]
    fn no_terminal_state_accepts_any_event() {
        for state in one_state_per_variant() {
            if !state.is_terminal() {
                continue;
            }
            for event in every_event() {
                refuses(&state, &event);
            }
        }
    }

    #[test]
    fn every_refusal_names_the_state_that_refused_and_the_event_refused() {
        for state in one_state_per_variant() {
            for event in every_event() {
                if let Err(error) = apply(&state, &event) {
                    let Error::InvalidTransition { from, event: named } = &error else {
                        panic!(
                            "{} must refuse {} as an illegal transition, got {error:?}",
                            state.name(),
                            event.discriminant()
                        );
                    };
                    assert_eq!(
                        from,
                        state.name(),
                        "the refusal must name the state that refused"
                    );
                    assert_eq!(
                        named,
                        event.discriminant(),
                        "the refusal must name the event that was refused"
                    );
                }
            }
        }
    }

    /// Every legal move `apply` can make, written out by hand: the state that
    /// is asked, the event it is asked with, and the state it answers with.
    ///
    /// This is the whole of what a task may do, and the sweep below compares
    /// `apply` against it entry for entry. A transition exists only when it
    /// appears here and in a `from_*` helper: an arm added without an entry, an
    /// entry without its arm, or a move that starts landing somewhere else all
    /// fail `every_move_is_a_declared_one_or_a_refusal`, so a new legal
    /// transition has to be declared deliberately rather than fall out of a
    /// match arm someone widened. Which side of the comparison reports the pair
    /// says which of the two drifted.
    ///
    /// Four terminal states appear nowhere in the list because they accept
    /// nothing, which is also what `no_terminal_state_accepts_any_event` says.
    ///
    /// The payloads are the ones `one_state_per_variant` and `every_event`
    /// carry, so an entry states what a pair of *variants* does with those
    /// payloads. Legality that turns on an attempt number, a commit or a phase
    /// rather than on the variant pair is a separate claim with its own tests —
    /// see `evidence_names_the_attempt_it_belongs_to_or_is_refused` and
    /// `nothing_is_published_until_the_remote_is_read_back_holding_the_commit`.
    ///
    /// `AttemptRecorded` has no `Remediating` row for the same reason `PhaseEntered`
    /// has none: the table's state holds attempt 2 while the sweep's record names
    /// attempt 1, so that pair is a refusal. The move it does make — a record naming
    /// the attempt a remediation holds — is
    /// `an_attempt_record_says_what_was_and_moves_the_attempt_it_names`.
    ///
    /// The length is part of the declaration: a pair leaves this table only on
    /// a written decision that the machine no longer makes the move, and one
    /// pair has left it. `("Paused", "Paused", "Paused")` was a nested pause,
    /// which T028 decided is a mistake rather than a second wait — the refusal
    /// is asserted in `a_pause_above_a_pause_is_refused`, and ADR-0026 is why.
    const LEGAL: [(&str, &str, &str); 52] = [
        ("Queued", "TaskQueued", "Queued"),
        ("Queued", "PreflightStarted", "Preflight"),
        ("Queued", "Paused", "Paused"),
        ("Queued", "TaskCancelled", "Cancelled"),
        ("Queued", "RecoveryDecision", "Queued"),
        ("Preflight", "PreflightStarted", "Preflight"),
        ("Preflight", "PreflightPassed", "Preflight"),
        ("Preflight", "PreflightFailed", "Failed"),
        ("Preflight", "AttemptStarted", "Preflight"),
        ("Preflight", "PhaseEntered", "Running"),
        ("Preflight", "Paused", "Paused"),
        ("Preflight", "TaskCancelled", "Cancelled"),
        ("Preflight", "RecoveryDecision", "Preflight"),
        ("Running", "PhaseEntered", "Running"),
        ("Running", "AgentOutput", "Running"),
        ("Running", "AttemptRecorded", "Running"),
        ("Running", "VerifyPassed", "Publishing"),
        ("Running", "VerifyFailed", "Running"),
        ("Running", "TaskFailed", "Failed"),
        ("Running", "TaskCancelled", "Cancelled"),
        ("Running", "Paused", "Paused"),
        ("Running", "Interrupted", "Paused"),
        ("Running", "RecoveryDecision", "Running"),
        ("Remediating", "TaskFailed", "Failed"),
        ("Remediating", "TaskCancelled", "Cancelled"),
        ("Remediating", "Paused", "Paused"),
        ("Remediating", "RecoveryDecision", "Remediating"),
        ("Verifying", "VerifyPassed", "Publishing"),
        ("Verifying", "VerifyFailed", "Verifying"),
        ("Verifying", "AttemptRecorded", "Verifying"),
        ("Verifying", "TaskFailed", "Failed"),
        ("Verifying", "TaskCancelled", "Cancelled"),
        ("Verifying", "Paused", "Paused"),
        ("Verifying", "Interrupted", "Paused"),
        ("Verifying", "RecoveryDecision", "Verifying"),
        ("Publishing", "VerifyPassed", "Publishing"),
        ("Publishing", "PublishStarted", "Publishing"),
        ("Publishing", "PublishVerified", "PublishedVerified"),
        ("Publishing", "AttemptRecorded", "Publishing"),
        ("Publishing", "TaskFailed", "Failed"),
        ("Publishing", "TaskCancelled", "Cancelled"),
        ("Publishing", "Paused", "Paused"),
        ("Publishing", "Interrupted", "Paused"),
        ("Publishing", "RecoveryDecision", "Publishing"),
        ("PublishedVerified", "PublishVerified", "PublishedVerified"),
        ("PublishedVerified", "TaskDone", "Done"),
        ("PublishedVerified", "Paused", "Paused"),
        ("PublishedVerified", "RecoveryDecision", "PublishedVerified"),
        ("Paused", "Resumed", "Running"),
        ("Paused", "TaskCancelled", "Cancelled"),
        ("Paused", "GateAcknowledged", "Acknowledged"),
        ("Paused", "RecoveryDecision", "Running"),
    ];

    /// Runs every state against every event and insists the answer is one the
    /// table above gave in advance.
    ///
    /// Every pair the cross product holds is decided twice: an accepted pair
    /// contributes the move it made to the list that is compared with [`LEGAL`],
    /// and a refused pair is refused through `Error::InvalidTransition` naming
    /// both of them. So the sweep fails on a legal move nobody declared, on a
    /// declared move that was withdrawn or retargeted, and on a refusal that
    /// stopped naming what it refused — and passes for the 240 pairs on nothing
    /// but the table.
    #[test]
    fn every_move_is_a_declared_one_or_a_refusal() {
        let mut made = Vec::new();
        for state in one_state_per_variant() {
            for event in every_event() {
                match apply(&state, &event) {
                    Ok(moved) => made.push((state.name(), event.discriminant(), moved.name())),
                    Err(_) => refuses(&state, &event),
                }
            }
        }
        let mut declared = LEGAL.to_vec();
        made.sort_unstable();
        declared.sort_unstable();
        assert_eq!(
            made,
            declared,
            "{} moves were made against {} declared: the difference is a transition \
             that changed without both sides being written down",
            made.len(),
            declared.len()
        );
    }

    #[test]
    fn done_is_reached_only_by_the_event_that_closes_a_published_task() {
        for state in one_state_per_variant() {
            for event in every_event() {
                if let Ok(TaskState::Done) = apply(&state, &event) {
                    assert!(
                        matches!(event, EventKind::TaskDone { .. }),
                        "{} reached Done on {} instead of on TaskDone",
                        state.name(),
                        event.discriminant()
                    );
                }
            }
        }
    }

    #[test]
    fn a_task_closes_only_through_the_event_that_closes_it() {
        for state in one_state_per_variant() {
            for event in every_event() {
                let closed = match apply(&state, &event) {
                    Ok(closed) if closed.is_terminal() => closed,
                    _ => continue,
                };
                let opened = format!(
                    "{} closed as {} on {}",
                    state.name(),
                    closed.name(),
                    event.discriminant()
                );
                match closed.name() {
                    "Done" => assert!(matches!(event, EventKind::TaskDone { .. }), "{opened}"),
                    "Failed" => assert!(
                        matches!(
                            event,
                            EventKind::TaskFailed { .. } | EventKind::PreflightFailed { .. }
                        ),
                        "{opened}"
                    ),
                    "Cancelled" => {
                        assert!(matches!(event, EventKind::TaskCancelled { .. }), "{opened}");
                    }
                    "Acknowledged" => {
                        assert!(
                            matches!(event, EventKind::GateAcknowledged { .. }),
                            "{opened}"
                        );
                    }
                    other => panic!("{other} is terminal and no event closes a task as it"),
                }
            }
        }
    }

    #[test]
    fn a_run_can_always_be_stopped() {
        // Every state a run can still be moving through. Already standing
        // still is the one exception, and `a_pause_above_a_pause_is_refused`
        // says why stopping such a state again is refused rather than a second
        // wait it would then owe a resume for.
        for state in resumable_states() {
            assert!(
                apply(&state, &pause(PauseReason::Input)).is_ok(),
                "{} must accept a pause: a run that cannot be stopped cannot be \
                 interrupted safely",
                state.name()
            );
        }
    }

    #[test]
    fn an_event_whose_fact_the_state_cannot_hold_changes_nothing() {
        for (state, event) in [
            (TaskState::Queued, queued()),
            (TaskState::Preflight, EventKind::PreflightStarted),
            (TaskState::Preflight, preflight_passed()),
            (TaskState::Preflight, started(1)),
            (working(1, Phase::Implement), output(1)),
            (publishing(1), publish_started(1)),
            (publishing(1), verify_passed(1)),
        ] {
            let expected = state.clone();
            moves(&state, &event, &expected);
        }
    }

    /// Which of the twelve a run is busy with, in the same order as
    /// [`one_state_per_variant`]: the six between starting and finishing
    /// (ADR-0027). [`active_states`] spells the same six out a second time from
    /// the variants themselves, so neither statement can pass by agreeing with
    /// the other.
    const ACTIVE: [bool; 12] = [
        false, true, true, true, true, true, true, false, false, false, false, false,
    ];

    /// The six active states, written from the variants rather than read out of
    /// [`ACTIVE`].
    fn active_states() -> Vec<TaskState> {
        vec![
            TaskState::Preflight,
            working(1, Phase::Implement),
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            TaskState::PublishedVerified {
                commit: "b7d1f3a".to_owned(),
            },
        ]
    }

    /// A queue holding each `(position, state)` pair, for asking the invariant
    /// about a set of tasks rather than about one state.
    fn queue(tasks: &[(u32, TaskState)]) -> BTreeMap<TaskId, TaskState> {
        tasks
            .iter()
            .map(|(position, state)| (TaskId::new(*position), state.clone()))
            .collect()
    }

    #[test]
    fn only_the_six_states_between_starting_and_finishing_take_the_active_slot() {
        // Asking a state whether it is active needs a second task to ask it
        // against: one task of any kind is a legal queue, so the answer only
        // shows up as a refusal. `Preflight` is the standing occupant.
        let at_work = TaskState::Preflight;
        for ((state, name), active) in one_state_per_variant()
            .iter()
            .zip(TASK_STATE_NAMES)
            .zip(ACTIVE)
        {
            let outcome = check_one_active(&queue(&[(1, state.clone()), (2, at_work.clone())]));
            assert_eq!(
                outcome.is_err(),
                active,
                "{name} paired with a task at work must {}be refused: {outcome:?}",
                if active { "" } else { "not " }
            );
            if active {
                assert!(
                    matches!(outcome, Err(Error::Policy { .. })),
                    "{name} was refused by something other than the queue's own \
                     rule: {outcome:?}"
                );
            }
        }
    }

    #[test]
    fn two_active_tasks_are_refused_and_the_error_names_both() {
        let active = active_states();
        assert_eq!(
            active.len(),
            6,
            "ADR-0027 fixes the number of active states at six"
        );
        for (position, first) in active.iter().enumerate() {
            for second in active.iter().skip(position + 1) {
                let refused = check_one_active(&queue(&[(2, first.clone()), (7, second.clone())]));
                let Err(Error::Policy { detail, paths }) = &refused else {
                    panic!(
                        "{} and {} active at once was not refused: {refused:?}",
                        first.name(),
                        second.name()
                    );
                };
                assert!(
                    paths.is_empty(),
                    "a row of the queue broke this rule, not a file: {paths:?}"
                );
                for (id, state) in [(2, first), (7, second)] {
                    assert!(
                        detail.contains(&format!("task {id} ({})", state.name())),
                        "the refusal must name task {id} in its own state: {detail}"
                    );
                }
                assert!(
                    detail.find("task 2") < detail.find("task 7"),
                    "the refusal must name the earlier task first: {detail}"
                );
            }
        }
    }

    #[test]
    fn the_refusal_names_every_active_task_and_not_only_the_first_two() {
        let waiting = [
            (4, TaskState::Preflight),
            (
                1,
                TaskState::Verifying {
                    attempt: AttemptId::new(1),
                },
            ),
            (
                9,
                TaskState::PublishedVerified {
                    commit: "b7d1f3a".to_owned(),
                },
            ),
        ];
        let refused = check_one_active(&queue(&waiting));
        let Err(Error::Policy { detail, .. }) = &refused else {
            panic!("three tasks active at once was not refused: {refused:?}");
        };
        assert!(
            detail.starts_with("3 tasks are active at once"),
            "the refusal opens with how many broke the rule: {detail}"
        );
        let mut named = waiting
            .iter()
            .map(|(id, state)| {
                detail
                    .find(&format!("task {id} ({})", state.name()))
                    .unwrap_or_else(|| panic!("the refusal omitted task {id}: {detail}"))
            })
            .collect::<Vec<_>>();
        named.sort_unstable();
        assert_eq!(
            named.iter().collect::<Vec<_>>(),
            vec![&named[0], &named[1], &named[2]],
            "the refusal must name them in id order, so the same broken queue \
             always reads the same way: {detail}"
        );
    }

    #[test]
    fn one_active_task_and_a_queue_of_waiting_or_stopped_ones_is_allowed() {
        let states = [
            (1, TaskState::Queued),
            (2, TaskState::Queued),
            (
                3,
                TaskState::Paused {
                    reason: PauseReason::Limit { until: None },
                    resume_to: Box::new(TaskState::Preflight),
                },
            ),
            (
                4,
                TaskState::Paused {
                    reason: PauseReason::HumanGate,
                    resume_to: Box::new(working(1, Phase::Green)),
                },
            ),
            (
                5,
                TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    resume_to: Box::new(TaskState::PublishedVerified {
                        commit: "b7d1f3a".to_owned(),
                    }),
                },
            ),
            (
                6,
                TaskState::Failed {
                    class: FailureClass::ProviderLimit,
                    detail: "429 from the provider".to_owned(),
                },
            ),
            (7, TaskState::Cancelled),
            (8, TaskState::Done),
            (9, working(3, Phase::Green)),
        ];
        let worked = check_one_active(&queue(&states));
        assert!(
            worked.is_ok(),
            "one task working among waiting, paused and finished ones is the queue \
             every run spends most of its time in: {worked:?}"
        );
        let empty = check_one_active(&BTreeMap::new());
        assert!(
            empty.is_ok(),
            "a queue with nothing in it has nothing running: {empty:?}"
        );
    }

    #[test]
    fn a_pause_is_not_active_whatever_it_would_resume_into() {
        let stopped_at_a_gate = TaskState::Paused {
            reason: PauseReason::HumanGate,
            resume_to: Box::new(working(2, Phase::Implement)),
        };
        let stopped = check_one_active(&queue(&[(1, stopped_at_a_gate), (2, TaskState::Queued)]));
        assert!(
            stopped.is_ok(),
            "a queue stopped at a pause has nothing running; naming the paused task \
             as the one that is would blame the only task that is not: {stopped:?}"
        );
    }

    /// Which of the twelve clear the way for a later task, in the same order as
    /// [`one_state_per_variant`]: work the remote was proved to hold, work that
    /// closed, work a human acknowledged, and work a human dropped. ADR-0028 held
    /// `Acknowledged` back as a question it would not answer alone; ADR-0031
    /// answers it, and VISION.md §3 invariant 2 names `acknowledged` as exactly
    /// what a gate's predecessor has to reach.
    const CLEARED: [bool; 12] = [
        false, false, false, false, false, false, true, true, true, false, false, true,
    ];

    #[test]
    fn a_queued_predecessor_blocks_its_successor_and_is_named() {
        let states = queue(&[(3, TaskState::Queued), (4, TaskState::Queued)]);
        let refused = check_predecessor(&states, TaskId::new(4));
        let Err(Error::Policy { detail, paths }) = &refused else {
            panic!("task 4 was allowed to start while task 3 was still queued: {refused:?}");
        };
        assert!(
            detail.starts_with("task 4 cannot start"),
            "the refusal opens by naming the task it refused to start: {detail}"
        );
        assert!(
            detail.contains("task 3 (Queued)"),
            "the refusal must name the predecessor that blocks, with the state it is \
             stuck in: {detail}"
        );
        assert!(
            paths.is_empty(),
            "a row of the queue broke this rule, not a file: {paths:?}"
        );
    }

    #[test]
    fn only_the_four_cleared_states_let_a_successor_start() {
        // Each variant is asked as task 1, against a task 2 that is itself still
        // queued: a predecessor's answer must come from the predecessor's own
        // state, and a check that read the successor's row would refuse every
        // task in a freshly imported plan.
        for ((state, name), cleared) in one_state_per_variant()
            .iter()
            .zip(TASK_STATE_NAMES)
            .zip(CLEARED)
        {
            let outcome = check_predecessor(
                &queue(&[(1, state.clone()), (2, TaskState::Queued)]),
                TaskId::new(2),
            );
            assert_eq!(
                outcome.is_err(),
                !cleared,
                "a predecessor in {name} must {}block task 2: {outcome:?}",
                if cleared { "not " } else { "" }
            );
            if !cleared {
                assert!(
                    matches!(outcome, Err(Error::Policy { .. })),
                    "{name} was refused by something other than the queue's own \
                     rule: {outcome:?}"
                );
            }
        }
    }

    #[test]
    fn published_closed_acknowledged_or_cancelled_work_lets_the_successor_start() {
        for predecessor in [
            TaskState::Done,
            acknowledged_state(),
            TaskState::Cancelled,
            published("b7d1f3a"),
        ] {
            let states = queue(&[(1, predecessor.clone()), (2, TaskState::Queued)]);
            let allowed = check_predecessor(&states, TaskId::new(2));
            assert!(
                allowed.is_ok(),
                "a predecessor in {} is work the queue may proceed past: {allowed:?}",
                predecessor.name()
            );
        }
    }

    #[test]
    fn a_gate_clears_the_way_only_once_a_human_has_acknowledged_it() {
        // The same queue entry either side of `ack`: the pause a run stops at is
        // not yet work the queue may proceed past, and the acknowledgement is.
        // A gate produces no commit, so this is the only state in which the work
        // behind it can ever be released (VISION.md §6, ADR-0031).
        let at_the_gate = parked(TaskState::Queued, PauseReason::HumanGate);
        let held = check_predecessor(
            &queue(&[(1, at_the_gate.clone()), (2, TaskState::Queued)]),
            TaskId::new(2),
        );
        assert!(
            matches!(held, Err(Error::Policy { .. })),
            "a gate the run stopped at, which no human has passed yet, must hold its \
             successor: {held:?}"
        );

        let passed = apply(&at_the_gate, &acknowledged())
            .expect("a pause that stopped at a gate is closed by an acknowledgement");
        assert_eq!(
            passed,
            acknowledged_state(),
            "the acknowledgement records who passed the gate and when, which is what \
             invariant 7 says a gate is closed by"
        );
        let released = check_predecessor(
            &queue(&[(1, passed), (2, TaskState::Queued)]),
            TaskId::new(2),
        );
        assert!(
            released.is_ok(),
            "the same entry, acknowledged, is the terminal success a successor waits \
             on: {released:?}"
        );
    }

    #[test]
    fn the_check_looks_only_at_the_tasks_below_the_successor() {
        let states = queue(&[
            (1, TaskState::Done),
            (2, published("b7d1f3a")),
            (3, TaskState::Cancelled),
            (4, TaskState::Queued),
            (5, working(1, Phase::Implement)),
            (6, TaskState::Queued),
        ]);
        let allowed = check_predecessor(&states, TaskId::new(4));
        assert!(
            allowed.is_ok(),
            "task 4's own row and the tasks after it are not its predecessors; \
             refusing on them would leave nothing in the queue startable: {allowed:?}"
        );
    }

    #[test]
    fn the_first_task_in_the_queue_has_nothing_to_wait_for() {
        let states = queue(&[
            (1, TaskState::Queued),
            (2, working(1, Phase::Green)),
            (3, published("b7d1f3a")),
        ]);
        let allowed = check_predecessor(&states, TaskId::new(1));
        assert!(
            allowed.is_ok(),
            "the head of the queue has no predecessor, so whatever follows it \
             cannot hold it back: {allowed:?}"
        );
        let empty = check_predecessor(&BTreeMap::new(), TaskId::new(5));
        assert!(
            empty.is_ok(),
            "a queue with nothing in it has no unpublished work below task 5: {empty:?}"
        );
    }

    #[test]
    fn the_refusal_names_every_unpublished_predecessor_in_id_order() {
        let waiting = [
            (4, TaskState::Preflight),
            (1, verifying(1)),
            (6, parked(TaskState::Queued, PauseReason::Blocked)),
        ];
        let mut states = queue(&waiting);
        states.insert(TaskId::new(3), TaskState::Cancelled);
        let refused = check_predecessor(&states, TaskId::new(7));
        let Err(Error::Policy { detail, .. }) = &refused else {
            panic!("three unpublished predecessors were not refused: {refused:?}");
        };
        assert!(
            !detail.contains("task 3"),
            "a cancelled predecessor is cleared work and must not be blamed for \
             holding the successor back: {detail}"
        );
        let mut named = waiting
            .iter()
            .map(|(id, state)| {
                detail
                    .find(&format!("task {id} ({})", state.name()))
                    .unwrap_or_else(|| panic!("the refusal omitted task {id}: {detail}"))
            })
            .collect::<Vec<_>>();
        named.sort_unstable();
        assert_eq!(
            named.iter().collect::<Vec<_>>(),
            vec![&named[0], &named[1], &named[2]],
            "the refusal must name them in id order, so the same blocked queue \
             always reads the same way: {detail}"
        );
    }

    #[test]
    fn a_successor_the_queue_does_not_hold_is_answered_from_what_lies_below_it() {
        let cleared = check_predecessor(
            &queue(&[(1, TaskState::Done), (2, TaskState::Cancelled)]),
            TaskId::new(9),
        );
        assert!(
            cleared.is_ok(),
            "task 9 need not be a row of the projection for the question to be \
             answered: {cleared:?}"
        );
        let refused = check_predecessor(
            &queue(&[(1, TaskState::Done), (5, publishing(1))]),
            TaskId::new(9),
        );
        assert!(
            matches!(refused, Err(Error::Policy { .. })),
            "an unpublished predecessor below an id the queue does not hold is still \
             a refusal: {refused:?}"
        );
    }
}

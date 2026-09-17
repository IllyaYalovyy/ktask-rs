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

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::classify::FailureClass;
use crate::error::{Error, Result};
use crate::event::EventKind;
use crate::ids::AttemptId;

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
/// guess. A second pause nests rather than replaces — the run then needs one
/// resume per reason, which is the honest reading of two separate waits.
///
/// `reason` is consulted once, for the one event that depends on it: a gate is
/// acknowledged by a person, so only a pause that stopped *at* a gate may be
/// closed by an acknowledgement (VISION.md §6). A pause is never a failure
/// either: `TaskFailed` describes work that will not get done, and a paused run
/// has not been asked whether it can.
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
        EventKind::Paused { reason: again } => Ok(parked(waiting, again.clone())),
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
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Interrupted { .. } => Err(refused(FROM, event)),
    }
}

#[cfg(test)]
mod tests {
    use super::{PauseReason, Phase, PhaseEntry, Recovery, Stream, TaskState, apply, phase_entry};
    use crate::{AttemptId, Error, EventKind, FailureClass};
    use serde::de::DeserializeOwned;
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
                reason: PauseReason::Input,
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

    /// Every catalog entry a journal can hold, carrying what a run would carry.
    /// Written out by hand rather than generated because the point of the list
    /// is that a person named each entry — and because a sweep over it is what
    /// proves no state stays quiet about an event.
    fn every_event() -> [EventKind; 19] {
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

    #[test]
    fn nothing_is_done_except_of_the_commit_the_remote_holds() {
        moves(
            &published(CANDIDATE),
            &task_done(CANDIDATE),
            &TaskState::Done,
        );
        refuses(&published(CANDIDATE), &task_done("0000000"));
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            working(1, Phase::Green),
            verifying(1),
            publishing(1),
            parked(verifying(1), PauseReason::Interrupted),
        ] {
            refuses(&state, &task_done(CANDIDATE));
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
    fn a_second_pause_keeps_where_the_first_one_was_waiting() {
        let once = parked(working(1, Phase::Green), PauseReason::Interrupted);
        let twice = parked(once.clone(), PauseReason::Limit { until: None });
        moves(
            &once,
            &pause(PauseReason::Limit { until: None }),
            &twice.clone(),
        );
        moves(&twice, &EventKind::Resumed, &once);
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
        for state in one_state_per_variant() {
            if state.is_terminal() {
                continue;
            }
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
}

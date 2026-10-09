//! Labelling one step's own outcome and reason, and the still-running line for a step that
//! has not ended yet — [`super::status`]'s own lowest-level work, pulled out of it so that
//! file stays within the workspace's function-length limit.

use std::time::{Duration, SystemTime};

use crate::{AttemptEnd, Clock, Outcome, Task, TaskStatus, WaitReason};

use super::{AttemptLine, AttemptOutcome, OutputActivity, StatusEntry, StepLine, Wait};
use crate::{IMPLEMENTATION, RESOLVE_STEP, REVIEW_STEP, TEST_STEP};

/// The outcome and reason shown for the implementation step ended at `end`, given what the
/// agent itself reported for it, when it reported anything at all; `answer`, when the report
/// was `needs-input` and this attempt was since given one, is appended to the question it
/// asked.
fn ended_outcome(
    end: &AttemptEnd,
    reported: Option<(Outcome, Option<String>)>,
    answer: Option<&str>,
) -> (AttemptOutcome, Option<String>) {
    match reported {
        // A provider can claim success — or, for the resolve step, a retry verdict, which
        // the runner treats the same way internally — while the runner rejects its
        // invocation on an independently recorded fact, such as using a different model
        // than the one asked for. The journal retains that provider report, but status must
        // show the attempt's real failed result and why, never a verdict that did not stand.
        Some((Outcome::Done | Outcome::Approved | Outcome::Accepted | Outcome::Retry, _))
            if end.status != TaskStatus::Done =>
        {
            (AttemptOutcome::Failed, end.reason.clone())
        }
        Some((Outcome::NeedsInput, reason)) => (
            AttemptOutcome::Reported(Outcome::NeedsInput),
            crate::attempt::with_answer(reason, answer),
        ),
        Some((outcome, reason)) => (AttemptOutcome::Reported(outcome), reason),
        None => (AttemptOutcome::Unreported, end.reason.clone()),
    }
}

/// The outcome and reason shown for a step named `name` that ended at `end`: the
/// implementation, review and test steps are judged by what the agent itself reported, when
/// it reported anything — see [`ended_outcome`] for `answer`; every other step — the sync,
/// the health check, the commit step and the push step, today — is a command-kind step the
/// tool itself ran, shown [`AttemptOutcome::Passed`] when `end.status` is `done`, or
/// [`AttemptOutcome::Failed`] with why, for the steps that can still end an attempt badly
/// because they run inside the attempt itself: the commit step and the push step.
pub(super) fn step_outcome(
    name: &str,
    end: &AttemptEnd,
    reported: Option<(Outcome, Option<String>)>,
    answer: Option<&str>,
) -> (AttemptOutcome, Option<String>) {
    if name == IMPLEMENTATION || name == REVIEW_STEP || name == TEST_STEP || name == RESOLVE_STEP {
        ended_outcome(end, reported, answer)
    } else if end.status == TaskStatus::Done {
        (AttemptOutcome::Passed, end.reason.clone())
    } else {
        (AttemptOutcome::Failed, end.reason.clone())
    }
}

/// The provider named for a step called `name`, given the provider the attempt ran with, when
/// one is known: the implementation, review and test steps are run by an agent, and name it;
/// every other step — the sync, health check, commit and push steps, today — is run by the
/// tool itself, and names none.
pub(super) fn step_provider(name: &str, provider: Option<&str>) -> Option<String> {
    if name == IMPLEMENTATION || name == REVIEW_STEP || name == TEST_STEP || name == RESOLVE_STEP {
        provider.map(str::to_owned)
    } else {
        None
    }
}

/// The session named for a step called `name`, given the session the attempt's provider
/// reported, when one is known: only the implementation step ever resumes one, so only it ever
/// names one here.
pub(super) fn step_session(name: &str, session: Option<&str>) -> Option<String> {
    if name == IMPLEMENTATION {
        session.map(str::to_owned)
    } else {
        None
    }
}

/// A step's own identity in a status line: its name, the model it ran with (the one requested,
/// when nothing more specific is known), the provider that ran it, and the session it
/// reported, before [`step_provider`] and [`step_session`] narrow provider and session to the
/// steps that actually show them.
#[derive(Clone, Copy)]
pub(super) struct StepIdentity<'a> {
    pub(super) name: &'a str,
    pub(super) provider: Option<&'a str>,
    pub(super) model: Option<&'a str>,
    pub(super) session: Option<&'a str>,
}

/// The still-running step line for `identity`, started at `started_at`: its elapsed time so
/// far, and whether it shows `running`, `waiting` (when `waiting_until` names a time not yet
/// passed) or `interrupted` depending on `run_alive`.
pub(super) fn running_step(
    identity: StepIdentity<'_>,
    started_at: SystemTime,
    waiting_until: Option<SystemTime>,
    waiting_reason: Option<WaitReason>,
    clock: &(impl Clock + ?Sized),
    run_alive: bool,
) -> StepLine {
    let elapsed = clock.now().duration_since(started_at).unwrap_or_default();
    let outcome = match (run_alive, waiting_until) {
        (false, _) => AttemptOutcome::Interrupted,
        (true, Some(_)) => AttemptOutcome::Waiting,
        (true, None) => AttemptOutcome::Running,
    };
    let waiting = live_wait(run_alive, waiting_until, waiting_reason, clock);
    let routed = waiting.as_ref().map(|wait| wait.reason.routed());
    StepLine {
        step: identity.name.to_owned(),
        provider: step_provider(identity.name, identity.provider),
        model: identity.model.map(str::to_owned),
        session: step_session(identity.name, identity.session),
        time_spent: elapsed,
        outcome,
        reason: None,
        findings: Vec::new(),
        waiting,
        limit_wait: None,
        limit_warning: None,
        usage: crate::Usage::default(),
        routed,
        more_time: None,
    }
}

/// What a live run's step is waiting for and how long is left, or none when it is not waiting
/// or its run is gone.
fn live_wait(
    run_alive: bool,
    until: Option<SystemTime>,
    reason: Option<WaitReason>,
    clock: &(impl Clock + ?Sized),
) -> Option<Wait> {
    if !run_alive {
        return None;
    }
    let remaining = until?.duration_since(clock.now()).ok()?;
    Some(Wait {
        reason: reason?,
        remaining,
    })
}

/// An [`AttemptLine`] numbered `number` carrying `current`'s own fields flat, the usage of every
/// one of `steps` summed, and the verdict of the last of them the router gave one.
pub(super) fn attempt_of(
    number: u32,
    current: StepLine,
    output_activity: Option<OutputActivity>,
    steps: Vec<StepLine>,
) -> AttemptLine {
    AttemptLine {
        number,
        step: current.step,
        provider: current.provider,
        model: current.model,
        session: current.session,
        time_spent: current.time_spent,
        outcome: current.outcome,
        reason: current.reason,
        findings: current.findings,
        waiting: current.waiting,
        limit_wait: current.limit_wait,
        limit_warning: current.limit_warning,
        output_activity,
        usage: steps.iter().fold(crate::Usage::default(), |total, step| {
            total.plus(step.usage)
        }),
        routed: steps.iter().rev().find_map(|step| step.routed),
        more_time: None,
        steps,
    }
}

/// The [`StatusEntry`] for `task`, given the step and reason a gate recorded stopping it before
/// any attempt began: one [`StepLine`] shown [`AttemptOutcome::Failed`], the same way a
/// command-kind step that fails inside an attempt is shown — `task.status` is untouched, still
/// `pending`.
pub(super) fn gate_stop_entry(task: Task, step: String, reason: String) -> StatusEntry {
    let line = StepLine {
        step,
        provider: None,
        model: None,
        session: None,
        time_spent: Duration::ZERO,
        outcome: AttemptOutcome::Failed,
        reason: Some(reason),
        findings: Vec::new(),
        waiting: None,
        limit_wait: None,
        limit_warning: None,
        usage: crate::Usage::default(),
        routed: None,
        more_time: None,
    };
    StatusEntry {
        task: task.id,
        title: task.title,
        status: task.status,
        attempt: attempt_of(0, line.clone(), None, vec![line]),
        history: Vec::new(),
        done_by_user: None,
    }
}

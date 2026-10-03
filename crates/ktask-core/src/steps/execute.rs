//! Running one step of an attempt already begun, and recording one that ran and passed before
//! the attempt itself began — the two places a step's own outcome is turned into the journal
//! events [`super::run_attempt_steps`] and [`super::run_one_attempt`] build on.

use std::time::{Duration, SystemTime};

use crate::{
    AttemptRun, Clock, Journal, LimitWait, Outcome, RunContext, RunError, TaskId, TaskStatus,
};

use super::known_cause::KnownCause;
use super::{Deps, PipelineState, PreStep, Step, StepOutcome};

/// Records a step, named `step`, that already ran and passed, in `duration`, with `reason` —
/// `Some` when it has something to say even though it passed — as one of the steps of attempt
/// `number` of task `id`'s own steps ahead of the list's own: begun and ended in the same call,
/// since it ran before the attempt itself was begun.
fn record_passed_step(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    step: &str,
    duration: Duration,
    reason: Option<&str>,
) -> Result<(), RunError> {
    crate::attempt::begin_step(journal, clock, id, number, step, None)?;
    crate::attempt::end_step(
        journal,
        clock,
        id,
        number,
        step,
        AttemptRun {
            duration,
            exit_code: Some(0),
            status: TaskStatus::Done,
            reason,
        },
        None,
        None,
        crate::Usage::default(),
        None,
    )?;
    Ok(())
}

/// `outcome`'s duration, exit code, status (`done` for [`StepOutcome::Passed`]), reason and
/// reported outcome, whichever of the two it is. Never called with [`StepOutcome::Waiting`]:
/// [`run_one_step`] acts on that one directly, without ever ending the step over it.
fn outcome_fields(
    outcome: StepOutcome,
) -> (
    Duration,
    Option<i32>,
    TaskStatus,
    Option<String>,
    Option<Outcome>,
) {
    match outcome {
        StepOutcome::Passed {
            duration,
            exit_code,
            reason,
            reported,
        } => (duration, exit_code, TaskStatus::Done, reason, reported),
        StepOutcome::Ended {
            duration,
            exit_code,
            status,
            reason,
            reported,
        } => (duration, exit_code, status, reason, reported),
        StepOutcome::Waiting { .. } => {
            unreachable!("run_one_step handles Waiting before outcome_fields is ever called")
        }
    }
}

/// Classifies `status` and `reason` against [`super::known_cause`]'s own known causes, when
/// `status` is not `done`: `Some` with the enriched what/why/fix message to record in `reason`
/// instead, when one matches — the signal [`PipelineState::known_cause`] carries up to
/// [`super::finish_attempt`], which leaves the task `pending` and skips the resolver entirely
/// over it.
fn classify_known_cause(
    status: TaskStatus,
    exit_code: Option<i32>,
    reason: Option<&str>,
) -> Option<String> {
    if !matches!(status, TaskStatus::Failed | TaskStatus::FailedUnknown) {
        return None;
    }
    let cause = KnownCause::classify(exit_code, reason)?;
    Some(cause.message(reason.unwrap_or_default()))
}

/// Records that `state`'s attempt is waiting on step `step` until `until`, then sleeps for
/// what is left of that wait — [`run_one_step`]'s own work for a [`StepOutcome::Waiting`],
/// pulled out of it so it stays within the workspace's function-length limit. Returns how long
/// it actually waited, so the step's own duration counts it, and [`run_one_step`] can fold it
/// into the [`LimitWait`] it records for the step once it ends.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn wait_for_limit(
    deps: &Deps<'_>,
    state: &PipelineState<'_>,
    step: &str,
    until: SystemTime,
) -> Result<Duration, RunError> {
    crate::attempt::record_waiting(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step,
        until,
    )?;
    let wait = until.duration_since(deps.clock.now()).unwrap_or_default();
    deps.sleep.sleep(wait);
    Ok(wait)
}

/// Ends step `step` with `outcome`, timed at `waited_so_far` (every earlier `Waiting`'s own
/// duration, the time actually spent waiting out its provider's usage limit included) plus
/// `outcome`'s own — enriching its reason and setting [`PipelineState::known_cause`] first,
/// when it matches one of [`super::known_cause`]'s own known causes — and recording
/// `limit_wait` against it, when the step waited for its provider's usage limit at least once
/// before it ended. [`run_one_step`]'s own tail, pulled out of it so it stays within the
/// workspace's function-length limit.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn end_one_step(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    waited_so_far: Duration,
    outcome: StepOutcome,
    limit_wait: Option<LimitWait>,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    let (duration, exit_code, status, mut reason, reported) = outcome_fields(outcome);
    let total = waited_so_far + duration;
    if let Some(message) = classify_known_cause(status, exit_code, reason.as_deref()) {
        reason = Some(message);
        state.known_cause = true;
    }
    let usage = std::mem::take(&mut state.usage);
    let used_model = state.used_model.take();
    crate::attempt::end_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step,
        AttemptRun {
            duration: total,
            exit_code,
            status,
            reason: reason.as_deref(),
        },
        reported,
        limit_wait,
        usage,
        used_model.as_deref(),
    )?;
    Ok((total, status, reason))
}

/// Begins `step`, already known enabled, then runs it — sleeping out and running it again, as
/// many times as it reports [`StepOutcome::Waiting`], never beginning a fresh step over it — and
/// ends it once it either passes or ends the attempt: its total duration (the time actually
/// spent running it, plus every wait for its provider's usage limit — counted in the task's own
/// time, even though it spent no attempt on it), the status it ended at, and, when that is not
/// `done`, why.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn run_one_step(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &dyn Step,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    crate::attempt::begin_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step.name(),
        step.model(context, state).as_deref(),
    )?;
    let mut total = Duration::ZERO;
    let mut limit_wait: Option<LimitWait> = None;
    loop {
        match step.run(deps, context, state)? {
            StepOutcome::Waiting { duration, until } => {
                total += duration;
                let waited = wait_for_limit(deps, state, step.name(), until)?;
                total += waited;
                let waited_so_far = limit_wait.map_or(Duration::ZERO, |wait| wait.waited);
                limit_wait = Some(LimitWait {
                    waited: waited_so_far + waited,
                    resumed_at: until,
                });
            }
            outcome => return end_one_step(deps, state, step.name(), total, outcome, limit_wait),
        }
    }
}

/// Records every one of `pre_steps` as attempt `number` of task `id`'s own first steps, in
/// order, via [`record_passed_step`]; their combined duration.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn record_pre_steps(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    pre_steps: &[PreStep],
) -> Result<Duration, RunError> {
    let mut total = Duration::ZERO;
    for pre_step in pre_steps {
        record_passed_step(
            journal,
            clock,
            id,
            number,
            pre_step.name,
            pre_step.duration,
            pre_step.reason.as_deref(),
        )?;
        total += pre_step.duration;
    }
    Ok(total)
}

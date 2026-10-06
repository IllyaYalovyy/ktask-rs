//! Running one step of an attempt already begun, and recording one that ran and passed before
//! the attempt itself began — the two places a step's own outcome is turned into the journal
//! events [`super::run_attempt_steps`] and [`super::run_one_attempt`] build on.

use std::time::{Duration, SystemTime};

use crate::{
    AttemptRun, Clock, Journal, LimitWait, Outcome, RunContext, RunError, TaskId, TaskStatus,
    WaitReason,
};

use super::known_cause::{KnownCause, codex_transport_reason};
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
    crate::attempt::begin_step(journal, clock, id, number, step, None, None)?;
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
        StepOutcome::Waiting { .. } | StepOutcome::TransportFailure { .. } => {
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
    reason: WaitReason,
) -> Result<Duration, RunError> {
    crate::attempt::record_waiting(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step,
        until,
        reason,
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
    let limit_warning = state.limit_warning.take();
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
        limit_warning.as_ref(),
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
    begin_one_step(deps, context, state, step)?;
    finish_started_step(deps, context, state, step)
}

/// The elapsed work and waits accumulated while one already-started step is rerun.
#[derive(Default)]
struct StepRunState {
    total: Duration,
    limit_wait: Option<LimitWait>,
    transport_failures: u32,
}

/// Repeats the already-started `step` until it ends, preserving all wait and transport retry
/// state on its one journal entry.
fn finish_started_step(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &dyn Step,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    let mut run = StepRunState::default();
    loop {
        let outcome = step.run(deps, context, state)?;
        if let Some(ended) =
            process_step_outcome(deps, context, state, step.name(), &mut run, outcome)?
        {
            return Ok(ended);
        }
    }
}

/// Acts on one fresh result from an already-started step; `Some` means the step has ended.
fn process_step_outcome(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    outcome: StepOutcome,
) -> Result<Option<(Duration, TaskStatus, Option<String>)>, RunError> {
    match outcome {
        StepOutcome::Waiting { duration, until } => {
            wait_for_provider_limit(deps, state, step, run, duration, until).map(|()| None)
        }
        StepOutcome::TransportFailure { duration, reason } => {
            retry_transport_failure(deps, context, state, step, run, duration, &reason)
        }
        outcome => end_one_step(deps, state, step, run.total, outcome, run.limit_wait).map(Some),
    }
}

/// Appends the start event for `step`, including the provider and model actually selected for
/// agent steps.
fn begin_one_step(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &PipelineState<'_>,
    step: &dyn Step,
) -> Result<(), RunError> {
    let agent_provider = matches!(
        step.name(),
        crate::IMPLEMENTATION | crate::REVIEW_STEP | crate::TEST_STEP | crate::RESOLVE_STEP
    )
    .then(|| deps.provider_for(step.name()).name.as_str());
    crate::attempt::begin_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step.name(),
        agent_provider,
        step.model(context, state).as_deref(),
    )?;
    Ok(())
}

/// Waits out one provider usage-limit response and keeps its wait record with prior waits for
/// the same step.
fn wait_for_provider_limit(
    deps: &Deps<'_>,
    state: &PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    duration: Duration,
    until: SystemTime,
) -> Result<(), RunError> {
    run.total += duration;
    let waited = wait_for_limit(deps, state, step, until, WaitReason::UsageLimit)?;
    run.total += waited;
    let waited_so_far = run.limit_wait.map_or(Duration::ZERO, |wait| wait.waited);
    run.limit_wait = Some(LimitWait {
        waited: waited_so_far + waited,
        resumed_at: until,
    });
    Ok(())
}

/// Retries one Codex transport failure after its bounded delay, or ends the step when the
/// configured consecutive-failure limit has been reached.
fn retry_transport_failure(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    duration: Duration,
    reason: &str,
) -> Result<Option<(Duration, TaskStatus, Option<String>)>, RunError> {
    run.total += duration;
    run.transport_failures = run.transport_failures.saturating_add(1);
    if run.transport_failures >= context.transport_retries {
        return exhausted_transport_failure(
            deps,
            state,
            step,
            run.total,
            run.transport_failures,
            reason,
            run.limit_wait,
        )
        .map(Some);
    }
    let wait = transport_backoff(run.transport_failures);
    let until = deps.clock.now() + wait;
    let waiting = WaitReason::TransportRetry {
        failure: run.transport_failures,
        limit: context.transport_retries,
    };
    run.total += wait_for_limit(deps, state, step, until, waiting)?;
    Ok(None)
}

/// Ends an agent step once its Codex stream has disconnected too many consecutive times.
fn exhausted_transport_failure(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    total: Duration,
    failures: u32,
    reason: &str,
    limit_wait: Option<LimitWait>,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    let exhausted = StepOutcome::Ended {
        duration: Duration::ZERO,
        exit_code: state.exit_code,
        status: TaskStatus::FailedUnknown,
        reason: Some(codex_transport_reason(failures, reason)),
        reported: None,
    };
    end_one_step(deps, state, step, total, exhausted, limit_wait)
}

/// The retry delays for an interrupted Codex stream: growing, but capped so an unattended run
/// always comes back to the operator in bounded time.
fn transport_backoff(failure: u32) -> Duration {
    Duration::from_secs(1_u64 << failure.saturating_sub(1).min(5))
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

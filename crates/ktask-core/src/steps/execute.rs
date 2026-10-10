//! Running one step of an attempt already begun: the one place a step's own outcome is turned
//! into the journal events [`super::run_attempt_steps`] and [`super::run_one_attempt`] build
//! on.

use std::time::{Duration, SystemTime};

use crate::queue_state::StepEnd;
use crate::route::{Facts, Route, Signals, route};
use crate::{AttemptRun, LimitWait, Outcome, Routed, RunContext, RunError, TaskStatus, WaitReason};

use super::{Deps, PipelineState, Step, StepOutcome};

/// A step's end, before it is recorded: what the step itself ended at, or — when the router
/// overrode them — what it routed them to.
struct Ending {
    duration: Duration,
    exit_code: Option<i32>,
    status: TaskStatus,
    reason: Option<String>,
    reported: Option<Outcome>,
}

/// Records that `state`'s attempt is waiting on step `step` until `until`, then sleeps for
/// what is left of that wait. Returns how long it actually waited, so the step's own duration
/// counts it.
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

/// Ends step `step` at `ending`, timed at `run.total` plus its own duration, with the verdict
/// the router last gave it, and `run`'s record of the usage-limit waits it sat out.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn end_one_step(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &StepRunState,
    ending: Ending,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    let total = run.total + ending.duration;
    let usage = std::mem::take(&mut state.usage);
    let used_model = state.used_model.take();
    let limit_warning = state.limit_warning.take();
    crate::attempt::end_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step,
        StepEnd {
            run: AttemptRun {
                duration: total,
                exit_code: ending.exit_code,
                status: ending.status,
                reason: ending.reason.as_deref(),
            },
            reported: ending.reported,
            limit_wait: run.limit_wait,
            limit_warning: limit_warning.as_ref(),
            usage,
            used_model: used_model.as_deref(),
            routed: run.routed,
        },
    )?;
    Ok((total, ending.status, ending.reason))
}

/// Begins `step`, already known enabled, then runs it — sleeping out and running it again, as
/// many times as the router says wait or retry, never beginning a fresh step over it — and
/// ends it once it either passes or ends the attempt: its total duration (the time actually
/// spent running it, plus every wait — counted in the task's own time, even though it spent no
/// attempt on it), the status it ended at, and, when that is not `done`, why.
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

/// The elapsed work, waits and retries accumulated while one already-started step is rerun.
#[derive(Default)]
struct StepRunState {
    total: Duration,
    limit_wait: Option<LimitWait>,
    retried: u32,
    routed: Option<Routed>,
}

/// Repeats the already-started `step` until it ends, preserving all wait and retry state on
/// its one journal entry.
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

/// Acts on one fresh result from an already-started step: a passed step ends; a failed one is
/// routed — [`crate::route::route`] is the only place that decides what happens to it. `Some`
/// means the step has ended.
fn process_step_outcome(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    outcome: StepOutcome,
) -> Result<Option<(Duration, TaskStatus, Option<String>)>, RunError> {
    let (mut ending, signals) = ending_of(outcome, run);
    let Some(signals) = signals else {
        return end_one_step(deps, state, step, run, ending).map(Some);
    };
    let facts = Facts {
        status: ending.status,
        exit_code: ending.exit_code,
        reason: ending.reason.as_deref(),
        reported: ending.reported,
        signals: &signals,
        retried: run.retried,
        transport_retries: context.transport_retries,
        now: deps.clock.now(),
        decider: step == crate::RESOLVE_STEP,
    };
    let route = route(&facts);
    apply_route(deps, state, step, run, route, &mut ending)
}

/// `Ending` for a step that passed, recording its own [`Routed`](crate::Routed) on `run`
/// directly when it carried one — the router never sees a step that did not fail.
fn passed_ending(
    duration: Duration,
    exit_code: Option<i32>,
    reason: Option<String>,
    reported: Option<Outcome>,
    routed: Option<Routed>,
    run: &mut StepRunState,
) -> Ending {
    if let Some(routed) = routed {
        run.routed = Some(routed);
    }
    Ending {
        duration,
        exit_code,
        status: TaskStatus::Done,
        reason,
        reported,
    }
}

/// How `outcome` ended its step, and the signals the router reads when it ended badly.
fn ending_of(outcome: StepOutcome, run: &mut StepRunState) -> (Ending, Option<Signals>) {
    match outcome {
        StepOutcome::Passed {
            duration,
            exit_code,
            reason,
            reported,
            routed,
        } => (
            passed_ending(duration, exit_code, reason, reported, routed, run),
            None,
        ),
        StepOutcome::Ended {
            duration,
            exit_code,
            status,
            reason,
            reported,
            signals,
        } => {
            let ending = Ending {
                duration,
                exit_code,
                status,
                reason,
                reported,
            };
            (ending, Some(signals))
        }
    }
}

/// Carries out `route` for a step that ended as `ending`: waits or retries — `None`, the step
/// runs again — or ends it, with the router's own words in place of its reason when it gave
/// some.
fn apply_route(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    route: Option<Route>,
    ending: &mut Ending,
) -> Result<Option<(Duration, TaskStatus, Option<String>)>, RunError> {
    if let Some(route) = &route {
        run.routed = Some(route.routed());
    }
    match route {
        Some(Route::Wait { until }) => {
            wait_and_run_again(deps, state, step, run, ending.duration, until)?;
            Ok(None)
        }
        Some(Route::Retry { n, of, after }) => {
            back_off_and_run_again(deps, state, step, run, ending.duration, (n, of, after))?;
            Ok(None)
        }
        Some(Route::Decide(decision)) => {
            if let Some(reason) = decision.reason.clone() {
                ending.reason = Some(reason);
            }
            state.decision = Some(decision);
            end_one_step(deps, state, step, run, take(ending)).map(Some)
        }
        Some(Route::Stop { reason, .. }) => {
            ending.reason = Some(reason);
            state.known_cause = true;
            end_one_step(deps, state, step, run, take(ending)).map(Some)
        }
        None => end_one_step(deps, state, step, run, take(ending)).map(Some),
    }
}

/// Waits `after` for retry `n` of `of`, and counts it, so the step can run again.
fn back_off_and_run_again(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    ran_for: Duration,
    (n, of, after): (u32, u32, Duration),
) -> Result<(), RunError> {
    run.retried = n;
    run.total += ran_for;
    let waiting = WaitReason::TransportRetry {
        failure: n,
        limit: of,
    };
    let until = deps.clock.now() + after;
    run.total += wait_for_limit(deps, state, step, until, waiting)?;
    Ok(())
}

/// Waits for the provider's usage limit to reset at `until` and counts the wait, so the step
/// can run again.
fn wait_and_run_again(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    run: &mut StepRunState,
    ran_for: Duration,
    until: SystemTime,
) -> Result<(), RunError> {
    run.total += ran_for;
    let waited = wait_for_limit(deps, state, step, until, WaitReason::UsageLimit)?;
    run.total += waited;
    let waited_so_far = run.limit_wait.map_or(Duration::ZERO, |wait| wait.waited);
    run.limit_wait = Some(LimitWait {
        waited: waited_so_far + waited,
        resumed_at: until,
    });
    Ok(())
}

/// `ending`, moved out, leaving a placeholder behind.
fn take(ending: &mut Ending) -> Ending {
    std::mem::replace(
        ending,
        Ending {
            duration: Duration::ZERO,
            exit_code: None,
            status: TaskStatus::Done,
            reason: None,
            reported: None,
        },
    )
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

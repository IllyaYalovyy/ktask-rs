//! Running one step of an attempt already begun, and recording one that ran and passed before
//! the attempt itself began — the two places a step's own outcome is turned into the journal
//! events [`super::run_attempt_steps`] and [`super::run_one_attempt`] build on.

use std::time::Duration;

use crate::{AttemptRun, Clock, Journal, Outcome, RunContext, RunError, TaskId, TaskStatus};

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
    )?;
    Ok(())
}

/// `outcome`'s duration, exit code, status (`done` for [`StepOutcome::Passed`]), reason and
/// reported outcome, whichever of the two it is.
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
    }
}

/// Begins, runs and ends one `step`, already known enabled: its duration, the status it ended
/// at, and, when that is not `done`, why.
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
    let (duration, exit_code, status, reason, reported) =
        outcome_fields(step.run(deps, context, state)?);
    crate::attempt::end_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step.name(),
        AttemptRun {
            duration,
            exit_code,
            status,
            reason: reason.as_deref(),
        },
        reported,
    )?;
    Ok((duration, status, reason))
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

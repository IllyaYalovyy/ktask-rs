//! Recording a step that already ran and passed before the attempt it belongs to was even
//! begun — the sync and health-check gates, when a project has switched them on — as that
//! attempt's own first steps. Pulled out of [`super::execute`] so that file stays within the
//! workspace's own file-length limit: unlike every other step there, these two begin and end
//! in the same call, since nothing here ever waits, retries or is routed.

use std::time::Duration;

use crate::queue_state::StepEnd;
use crate::{AttemptRun, Clock, Journal, RunError, TaskId, TaskStatus};

use super::PreStep;

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
        StepEnd {
            run: AttemptRun {
                duration,
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason,
            },
            reported: None,
            limit_wait: None,
            limit_warning: None,
            usage: crate::Usage::default(),
            used_model: None,
            routed: None,
        },
    )?;
    Ok(())
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

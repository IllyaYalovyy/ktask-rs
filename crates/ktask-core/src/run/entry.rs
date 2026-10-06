//! The public way to start a queue run.

use super::{
    RunEnd, RunError, RunReport, RunRequest, account_for_interrupted_run, attempt_loop, take_lock,
};
use crate::{Clock, Commands, Git, Journal, RunLock, SessionLog, Sleep};

/// Use case: runs the pending tasks of `request.context.project_name`, in queue order, one attempt
/// each, with the providers in `request` — stopping at the first task of kind `human`, at the first attempt
/// that ends at anything but `done`, `skipped` or `superseded`, at the first task in queue
/// order already left `failed`, `blocked` or `failed-unknown` (nothing is attempted in that
/// case), or when nothing is left pending.
///
/// Takes `lock` for the whole run, so that two runs of the same project never overlap. When
/// the previous run was killed while an attempt was in progress, this run finds its task
/// still `running`, ends it `failed-unknown` with the reason "the run was interrupted", and
/// stops there without attempting anything else.
///
/// # Errors
///
/// Fails, attempting nothing, when another run already holds `lock`. Fails when the journal
/// cannot be read or written; an attempt's own failure is reported in the returned
/// [`RunReport`], not here.
#[allow(clippy::too_many_arguments)]
pub fn run_queue(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    git: &impl Git,
    session_log: &impl SessionLog,
    sleep: &impl Sleep,
    lock: &impl RunLock,
    request: RunRequest<'_>,
) -> Result<RunReport, RunError> {
    take_lock(lock)?;
    if let Some(attempted) = account_for_interrupted_run(journal, clock)? {
        let end = RunEnd::Stopped {
            id: attempted.id,
            status: attempted.status,
        };
        return Ok(RunReport {
            attempted: vec![attempted],
            end,
        });
    }
    attempt_loop(
        journal,
        clock,
        commands,
        git,
        &request.providers,
        session_log,
        sleep,
        request.context,
    )
}

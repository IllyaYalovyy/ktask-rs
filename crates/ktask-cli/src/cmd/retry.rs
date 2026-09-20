//! Retry command: start a fresh attempt on a failed task.

use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
    _task: String,
) -> RunOutcome {
    RunOutcome::Drained
}

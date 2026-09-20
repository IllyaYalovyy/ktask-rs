//! Run command: drain the queue in order.

use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
    _task: Option<String>,
    _from: Option<String>,
) -> RunOutcome {
    RunOutcome::Drained
}

//! Resume command: continue from the first incomplete task.

use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
) -> RunOutcome {
    RunOutcome::Drained
}

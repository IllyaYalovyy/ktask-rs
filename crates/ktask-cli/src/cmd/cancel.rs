//! Cancel command: mark a task cancelled.

use ktask_core::RunOutcome;

pub(crate) fn run(_project: Option<ktask_core::Project>, _task: String) -> RunOutcome {
    RunOutcome::Drained
}

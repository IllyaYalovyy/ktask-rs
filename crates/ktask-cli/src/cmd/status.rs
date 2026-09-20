//! Status command: show the status of all tasks.

use ktask_core::RunOutcome;

pub(crate) fn run(_project: Option<ktask_core::Project>) -> RunOutcome {
    RunOutcome::Drained
}

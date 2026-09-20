//! Ack command: pass a human gate.

use ktask_core::RunOutcome;

pub(crate) fn run(_project: Option<ktask_core::Project>, _task: Option<String>) -> RunOutcome {
    RunOutcome::Drained
}

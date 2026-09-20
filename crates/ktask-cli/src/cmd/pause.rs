//! Pause command: pause the running queue.

use ktask_core::RunOutcome;

pub fn run(_project: Option<ktask_core::Project>) -> RunOutcome {
    RunOutcome::Drained
}

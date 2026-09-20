//! Interrupt command: terminate the running attempt.

use ktask_core::RunOutcome;

pub fn run(_project: Option<ktask_core::Project>) -> RunOutcome {
    RunOutcome::Drained
}

//! Add command: add a new task to the queue.

use ktask_core::RunOutcome;
use std::path::PathBuf;

pub(crate) fn run(_file: Option<PathBuf>) -> RunOutcome {
    RunOutcome::Drained
}

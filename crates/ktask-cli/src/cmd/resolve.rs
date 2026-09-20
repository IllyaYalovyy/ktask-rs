//! Resolve command: answer a `waiting_input` question.

use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _task: String,
    _note: Option<String>,
) -> RunOutcome {
    RunOutcome::Drained
}

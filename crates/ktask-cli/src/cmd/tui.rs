//! TUI command: launch the interactive interface.

use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
) -> RunOutcome {
    RunOutcome::Drained
}

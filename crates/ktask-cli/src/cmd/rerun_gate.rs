//! Rerun-gate command: re-run a completion gate.

use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
    _task: String,
    _gate: Option<String>,
) -> RunOutcome {
    RunOutcome::Drained
}

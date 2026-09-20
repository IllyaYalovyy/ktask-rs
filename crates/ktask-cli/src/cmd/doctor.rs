//! Doctor command: check provider availability, git, toolchain, etc.

use ktask_core::RunOutcome;

pub(crate) fn run(_json: bool) -> RunOutcome {
    RunOutcome::Drained
}

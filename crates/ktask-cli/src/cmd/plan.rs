//! Plan command: validate tasks in the queue.

use crate::cli::PlanSubcommand;
use ktask_core::RunOutcome;

pub(crate) fn run(
    _project: Option<ktask_core::Project>,
    _subcommand: PlanSubcommand,
) -> RunOutcome {
    RunOutcome::Drained
}

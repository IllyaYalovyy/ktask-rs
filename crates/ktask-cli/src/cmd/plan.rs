//! Plan command: validate tasks in the queue.

use ktask_core::RunOutcome;
use crate::cli::PlanSubcommand;

pub fn run(
    _project: Option<ktask_core::Project>,
    _subcommand: PlanSubcommand,
) -> RunOutcome {
    RunOutcome::Drained
}

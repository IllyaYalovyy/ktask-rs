//! `ktask-rs plan lint`: validates the whole queue without running anything.
//!
//! Reporting every malformed task in one pass is T113. [`run`] exists now,
//! as a placeholder, only so [`crate::cmd::dispatch`]'s match is exhaustive.

use crate::cli::PlanCommand;
use ktask_core::{Config, Project, RunOutcome};

/// Placeholder for the `plan` command; always [`RunOutcome::Drained`] until
/// T113.
pub(crate) fn run(_project: &Project, _config: &Config, _command: &PlanCommand) -> RunOutcome {
    RunOutcome::Drained
}

//! `ktask-rs status`: the headless dashboard.
//!
//! Printing per-task state and the summary line, in both human and `--json`
//! form, is T111. [`run`] exists now, as a placeholder, only so
//! [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, Project, RunOutcome};

/// Placeholder for the `status` command; always [`RunOutcome::Drained`]
/// until T111.
pub(crate) fn run(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

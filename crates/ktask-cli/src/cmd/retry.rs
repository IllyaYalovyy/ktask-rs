//! `ktask-rs retry`: starts a fresh remediation attempt for a failed task.
//!
//! Seeding the attempt with the failure bundle is T116. [`run`] exists now,
//! as a placeholder, only so [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, Project, RunOutcome, TaskId};

/// Placeholder for the `retry` command; always [`RunOutcome::Drained`] until
/// T116.
pub(crate) fn run(_project: &Project, _config: &Config, _task: TaskId) -> RunOutcome {
    RunOutcome::Drained
}

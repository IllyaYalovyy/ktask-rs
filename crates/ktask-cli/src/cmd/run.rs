//! `ktask-rs run`: drains the queue in order, strictly serially.
//!
//! Constructing the provider, subscribing to the bus, and streaming progress
//! and per-task results is T115. [`run`] exists now, as a placeholder, only
//! so [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, Project, RunOutcome, TaskId};

/// Placeholder for the `run` command; always [`RunOutcome::Drained`] until
/// T115.
pub(crate) fn run(
    _project: &Project,
    _config: &Config,
    _task: Option<TaskId>,
    _from: Option<TaskId>,
) -> RunOutcome {
    RunOutcome::Drained
}

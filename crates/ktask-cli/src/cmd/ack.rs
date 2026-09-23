//! `ktask-rs ack`: passes a human gate.
//!
//! Marking the gate satisfied and leaving the queue paused is T117. [`run`]
//! exists now, as a placeholder, only so [`crate::cmd::dispatch`]'s match is
//! exhaustive.

use ktask_core::{Config, Project, RunOutcome, TaskId};

/// Placeholder for the `ack` command; always [`RunOutcome::Drained`] until
/// T117.
pub(crate) fn run(_project: &Project, _config: &Config, _task: Option<TaskId>) -> RunOutcome {
    RunOutcome::Drained
}

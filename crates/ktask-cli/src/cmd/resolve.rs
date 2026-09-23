//! `ktask-rs resolve`: answers a `waiting_input` question.
//!
//! Writing the answer as an ADR and journaling the resolution is T117.
//! [`run`] exists now, as a placeholder, only so [`crate::cmd::dispatch`]'s
//! match is exhaustive.

use ktask_core::{Config, Project, RunOutcome, TaskId};

/// Placeholder for the `resolve` command; always [`RunOutcome::Drained`]
/// until T117.
pub(crate) fn run(
    _project: &Project,
    _config: &Config,
    _task: TaskId,
    _note: Option<&str>,
) -> RunOutcome {
    RunOutcome::Drained
}

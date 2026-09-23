//! `ktask-rs rerun-gate`: re-runs a gate against the current worktree,
//! discarding any cached result.
//!
//! Running the named gate (or the whole completion set) and journaling the
//! structured outcome is T120. [`run`] exists now, as a placeholder, only so
//! [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, GateKind, Project, RunOutcome, TaskId};

/// Placeholder for the `rerun-gate` command; always [`RunOutcome::Drained`]
/// until T120.
pub(crate) fn run(
    _project: &Project,
    _config: &Config,
    _task: TaskId,
    _gate: Option<GateKind>,
) -> RunOutcome {
    RunOutcome::Drained
}

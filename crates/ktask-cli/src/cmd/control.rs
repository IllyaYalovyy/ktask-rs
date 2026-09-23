//! `ktask-rs pause` / `ktask-rs interrupt` / `ktask-rs cancel`: control of a
//! run in progress, from any terminal.
//!
//! Signaling a running supervisor through a control file, and journaling
//! `Paused`, `Interrupted` and `TaskCancelled`, is T119. [`pause`],
//! [`interrupt`] and [`cancel`] exist now, as placeholders, only so
//! [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, Project, RunOutcome, TaskId};

/// Placeholder for the `pause` command; always [`RunOutcome::Drained`] until
/// T119.
pub(crate) fn pause(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

/// Placeholder for the `interrupt` command; always [`RunOutcome::Drained`]
/// until T119.
pub(crate) fn interrupt(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

/// Placeholder for the `cancel` command; always [`RunOutcome::Drained`]
/// until T119.
pub(crate) fn cancel(_project: &Project, _config: &Config, _task: TaskId) -> RunOutcome {
    RunOutcome::Drained
}

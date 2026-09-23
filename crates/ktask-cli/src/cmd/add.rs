//! `ktask-rs add`: opens `$EDITOR` with a task template, or reads `--file`.
//!
//! Implementing template/`$EDITOR` handling, validation and insertion into
//! the queue is T112. [`run`] exists now, as a placeholder, only so
//! [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, Project, RunOutcome};
use std::path::Path;

/// Placeholder for the `add` command; always [`RunOutcome::Drained`] until
/// T112.
pub(crate) fn run(_project: &Project, _config: &Config, _file: Option<&Path>) -> RunOutcome {
    RunOutcome::Drained
}

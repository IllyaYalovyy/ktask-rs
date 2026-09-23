//! `ktask-rs init`: registers the current repository.
//!
//! `main` already resolves (and, for `init`, registers) the project before
//! dispatch reaches here, so by the time [`run`] is called the project
//! exists. Printing its id and state directory, and making a second `init`
//! idempotently report the existing registration, is T108. [`run`] exists
//! now, as a placeholder, only so [`crate::cmd::dispatch`]'s match is
//! exhaustive.

use ktask_core::{Config, Project, RunOutcome};

/// Placeholder for the `init` command; always [`RunOutcome::Drained`] until
/// T108.
pub(crate) fn run(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

//! `ktask-rs resume`: continues from the first task that is not done.
//!
//! Finding the first incomplete task and running from it is T116. [`run`]
//! exists now, as a placeholder, only so [`crate::cmd::dispatch`]'s match is
//! exhaustive.

use ktask_core::{Config, Project, RunOutcome};

/// Placeholder for the `resume` command; always [`RunOutcome::Drained`]
/// until T116.
pub(crate) fn run(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

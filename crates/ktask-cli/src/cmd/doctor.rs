//! `ktask-rs doctor`: provider, git, toolchain and state-directory checks.
//!
//! Implementing the checks `docs/CONTRACT.md` section 3 documents is T110.
//! [`run`] exists now, as a placeholder, only so [`crate::cmd::dispatch`]'s
//! match is exhaustive.

use ktask_core::{Config, Project, RunOutcome};

/// Placeholder for the `doctor` command; always [`RunOutcome::Drained`]
/// until T110.
pub(crate) fn run(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

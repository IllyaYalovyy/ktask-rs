//! `ktask-rs tui`: launches the interface described in `docs/CONTRACT.md`
//! section 4. Requires a terminal.
//!
//! Wiring up `ktask-tui` is T131. [`run`] exists now, as a placeholder, only
//! so [`crate::cmd::dispatch`]'s match is exhaustive.

use ktask_core::{Config, Project, RunOutcome};

/// Placeholder for the `tui` command; always [`RunOutcome::Drained`] until
/// T131.
pub(crate) fn run(_project: &Project, _config: &Config) -> RunOutcome {
    RunOutcome::Drained
}

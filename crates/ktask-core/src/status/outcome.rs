//! The typed outcomes that status projections observe for attempts and steps.

use crate::Outcome;

/// How an attempt or step ended, without choosing words for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The step is currently running.
    Running,
    /// An agent reported this outcome.
    Reported(Outcome),
    /// The attempt ended without an agent report.
    Unreported,
    /// The journal says running but no process can finish it.
    Interrupted,
    /// A tool-run step passed.
    Passed,
    /// A tool-run step failed.
    Failed,
    /// The provider hit a usage limit and the step is waiting to resume.
    Waiting,
}

//! [`AttemptOutcome`]: how an attempt's, or one of its steps', outcome is labelled for
//! display — [`crate::status`]'s own vocabulary, shared by the queue screen.

use std::fmt;

use crate::{Outcome, TaskStatus};

/// How an attempt's outcome is labelled: as the agent itself reported it, or as the tool
/// observed it when the agent never reported at all — a crash, a kill past the time limit, or
/// a run left running by a killed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Still running: there is no outcome yet.
    Running,
    /// The agent's own reported outcome.
    Reported(Outcome),
    /// The tool observed the attempt end with no report from the agent.
    Unreported,
    /// The journal still calls the attempt running, but no run is alive to finish it: a run
    /// that was killed outright left it behind, and nothing has reconciled it yet.
    Interrupted,
    /// A command-kind step — the sync or the health check, say — ran and exited zero. Such a
    /// pre-attempt step is only ever journaled once it has already succeeded: a failing one
    /// stops the run before an attempt even begins, so this is the only outcome one is ever
    /// shown with.
    Passed,
    /// A command-kind step that runs inside the attempt itself — the commit step, today — did
    /// not succeed: `reason` on its line says why.
    Failed,
}

impl AttemptOutcome {
    /// The word the outcome is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Reported(outcome) => outcome.as_str(),
            Self::Unreported => TaskStatus::FailedUnknown.as_str(),
            Self::Interrupted => "interrupted",
            Self::Passed => "passed",
            Self::Failed => TaskStatus::Failed.as_str(),
        }
    }
}

impl fmt::Display for AttemptOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

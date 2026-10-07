//! The typed result produced by a single pipeline step.

use std::time::Duration;

use crate::route::Signals;
use crate::{Outcome, TaskStatus};

/// What running one step produced.
pub(crate) enum StepOutcome {
    /// It passed. `reason` is `Some` for a step with something worth recording even though it
    /// passed (the commit step's hash, say, "nothing was changed", or the resolve step's own
    /// note that `retry --reset-tree` reset the working tree); `None` for a step
    /// (implementation, review, test) that says nothing beyond passing, and for a resolve step
    /// whose `retry` did not ask for the tree to be reset.
    Passed {
        /// How long it took.
        duration: Duration,
        /// Its own exit code, when it ran a process.
        exit_code: Option<i32>,
        /// What it has to say even though it passed.
        reason: Option<String>,
        /// The agent's own fine-grained outcome, when this step ran a provider and it reported
        /// one.
        reported: Option<Outcome>,
    },
    /// It ended the whole attempt right here: `status` other than `done`, why, and — for a
    /// step that ran a provider and it reported an outcome of its own — what.
    Ended {
        /// How long it took.
        duration: Duration,
        /// Its own exit code, when it ran a process.
        exit_code: Option<i32>,
        /// What the attempt, and so the task, ends at.
        status: TaskStatus,
        /// Why.
        reason: Option<String>,
        /// The agent's own fine-grained outcome, when this step ran a provider and it reported
        /// one.
        reported: Option<Outcome>,
        /// What the provider run showed besides its exit code: the facts the router reads.
        /// Empty for a step that ran no provider.
        signals: Signals,
    },
}

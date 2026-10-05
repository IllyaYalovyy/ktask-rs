//! The typed result produced by a single pipeline step.

use std::time::{Duration, SystemTime};

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
    },
    /// The provider's own output said its usage limit was hit: the step neither passed nor
    /// ended the attempt. [`super::execute::run_one_step`] records this as a wait, sleeps until
    /// `until`, then runs the step itself again — never beginning a fresh step, so the
    /// attempt's own number never moves for it, exactly as a hand-driven retry would.
    Waiting {
        /// How long the provider ran before its output showed the limit.
        duration: Duration,
        /// The time to wait until before trying again: the provider's own message named it, or
        /// the resolve role's own default back-off when it did not.
        until: SystemTime,
        /// Why the step is waiting, shown while the wait is live.
        reason: String,
    },
    /// Codex exhausted its stream transport. The runner retries this same step with a bounded
    /// back-off, or turns the last consecutive failure into a pending known cause.
    TransportFailure {
        /// How long the failed provider invocation ran.
        duration: Duration,
        /// Codex's observed transport error.
        reason: String,
    },
}

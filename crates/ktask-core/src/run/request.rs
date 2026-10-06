//! Everything the caller chooses for one queue run.

use super::{RunContext, TaskProviders};

/// What a queue run is asked to do: the providers it may use and the context it executes in.
/// A new choice the caller makes is a new field here, not a new entry point.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    /// The providers the run chooses between.
    pub providers: TaskProviders<'a>,
    /// Where the run executes.
    pub context: RunContext<'a>,
}

//! Everything the caller chooses for one queue run.

use super::{RunContext, TaskProviders};
use crate::AttemptOutput;

/// What a queue run is asked to do: the providers it may use and the context it executes in.
/// A new choice the caller makes is a new field here, not a new entry point.
#[derive(Clone, Copy)]
pub struct RunRequest<'a> {
    /// The providers the run chooses between.
    pub providers: TaskProviders<'a>,
    /// Where the run executes.
    pub context: RunContext<'a>,
    /// Where the run reads back when an attempt last wrote output.
    pub output: &'a dyn AttemptOutput,
}

impl std::fmt::Debug for RunRequest<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunRequest")
            .field("providers", &self.providers)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

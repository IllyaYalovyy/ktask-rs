//! Everything the caller chooses for one queue run, and the ports it runs against.

use super::{RunContext, TaskProviders};
use crate::{AttemptOutput, Clock, Commands, Git, InstructionFiles, Journal, SessionLog, Sleep};

/// The ports a queue run needs before its providers, output destination and instructions are
/// chosen — [`super::run_queue`] and [`RunRequest`] between them take every input a run needs.
#[derive(Clone, Copy)]
pub struct RunPorts<'a> {
    /// Where the run reads and appends events.
    pub journal: &'a dyn Journal,
    /// What the run reads the time from.
    pub clock: &'a dyn Clock,
    /// How the run executes a command-kind step.
    pub commands: &'a dyn Commands,
    /// How the run reads and changes the project's working tree.
    pub git: &'a dyn Git,
    /// Where the run records each step's own session, for a provider that supports resuming
    /// one.
    pub session_log: &'a dyn SessionLog,
    /// What the run sleeps on while a step waits out a limit or a back-off.
    pub sleep: &'a dyn Sleep,
}

impl std::fmt::Debug for RunPorts<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RunPorts").finish_non_exhaustive()
    }
}

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
    /// Where the run reads the instruction files its agents' prompts open with.
    pub instruction_files: &'a dyn InstructionFiles,
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

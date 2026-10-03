//! The status use case: reads journal history into the facts an interface needs to show it.

use std::time::Duration;

use crate::{AttemptOutput, Clock, Journal, JournalError, RunLock, list_all_tasks};

mod build;
mod facts;
mod lines;
mod outcome;

pub use facts::{AttemptLine, DoneMark, OutputActivity, StatusEntry, StepLine};
pub use outcome::AttemptOutcome;

/// Reads every task with recorded status history, in queue order.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when the run lock cannot be used.
pub fn status(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
) -> Result<Vec<StatusEntry>, JournalError> {
    status_inner(journal, clock, lock, None, Duration::MAX)
}

/// Reads status history and the live provider-output facts for running attempts.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when the run lock cannot be used.
pub fn status_with_output(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: &impl AttemptOutput,
    silent_after: Duration,
) -> Result<Vec<StatusEntry>, JournalError> {
    status_inner(journal, clock, lock, Some(output), silent_after)
}

/// The journal read shared by the status views.
fn status_inner(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: Option<&dyn AttemptOutput>,
    silent_after: Duration,
) -> Result<Vec<StatusEntry>, JournalError> {
    let run_alive = match crate::attempt::running(journal)? {
        Some(_) => lock
            .in_progress()
            .map_err(|error| JournalError::new(error.to_string()))?,
        None => false,
    };
    let mut entries = Vec::new();
    for task in list_all_tasks(journal)? {
        if let Some(entry) =
            build::entry_for_task(journal, task, clock, run_alive, output, silent_after)?
        {
            entries.push(entry);
        }
    }
    Ok(entries)
}

//! Attempts: beginning one, recording what happened to it, marking it running, ending it, and
//! reading it back.
//!
//! The rules live in [`crate::queue_state`], the one place a command is decided against the
//! state its events fold to; this is the thin layer — mirroring [`crate::task`]'s — that reads
//! the journal, decides, and appends, or reads and queries.

use std::time::SystemTime;

use crate::journal::AttemptRun;
use crate::queue_state::{decide_and_append, read_and_query};
use crate::{
    Attempt, BeginAttemptError, Clock, Event, Journal, JournalError, Outcome, RecordReportError,
    TaskId,
};

/// Use case: starts the next attempt at the pending task numbered `id`, marking it running.
/// Returns the attempt's number, starting at 1 and never reused for this task.
///
/// # Errors
///
/// Fails, recording nothing, when there is no such task, when it is not pending, or when the
/// journal cannot be read or written.
pub(crate) fn begin_attempt(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
) -> Result<u32, BeginAttemptError> {
    let at = clock.now();
    decide_and_append(journal, |state| state.decide_begin_attempt(id, at))
}

/// Use case: records `outcome` (and `reason`) for attempt `number` of task `id`. A later
/// report for the same running attempt is recorded the same way and stands as the current
/// one; both stay in the journal.
///
/// # Errors
///
/// Fails, recording nothing, when no attempt numbered `number` was started for this task, when
/// it was but has since ended, or when the journal cannot be read or written.
pub(crate) fn record_report(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    number: u32,
    outcome: Outcome,
    reason: Option<&str>,
) -> Result<(), RecordReportError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state
            .decide_record_report(id, number, outcome, reason, at)
            .map(|event| (vec![event], ()))
    })
}

/// Use case: begins the next attempt at the pending task numbered `id` and, in the same
/// append, marks it running with `provider` — for a caller (the `run` use case) that always
/// knows its provider up front, so "begun" and "running" are never visible apart, one journal
/// round trip apart from each other. Returns the attempt's number.
///
/// # Errors
///
/// Fails, recording nothing, when there is no such task, when it is not pending, or when the
/// journal cannot be read or written.
pub(crate) fn begin_attempt_running(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    provider: &str,
) -> Result<u32, BeginAttemptError> {
    let at = clock.now();
    let provider = provider.to_owned();
    decide_and_append(journal, move |state| {
        let (mut events, number) = state.decide_begin_attempt(id, at)?;
        events.push(Event::AttemptRunning {
            id,
            number,
            provider: provider.clone(),
            at,
        });
        Ok((events, number))
    })
}

/// Use case: ends attempt `number` of task `id` with `run`, setting the task to `run.status`.
///
/// # Errors
///
/// Fails, changing nothing, when no attempt numbered `number` is running for this task, or when
/// the journal cannot be read or written.
pub(crate) fn end_attempt(
    journal: &impl Journal,
    id: TaskId,
    number: u32,
    run: AttemptRun<'_>,
    at: SystemTime,
) -> Result<(), RecordReportError> {
    decide_and_append(journal, |state| {
        state
            .decide_end_attempt(id, number, run, at)
            .map(|event| (vec![event], ()))
    })
}

/// The task currently running, and its current attempt's number — a project has at most one at
/// a time. `None` when none is running.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn running(journal: &impl Journal) -> Result<Option<(TaskId, u32)>, JournalError> {
    read_and_query(journal, crate::queue_state::QueueState::running)
}

/// The most recent attempt at task `id`. `None` when it was never attempted.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_attempt(
    journal: &impl Journal,
    id: TaskId,
) -> Result<Option<Attempt>, JournalError> {
    read_and_query(journal, |state| state.attempt_of(id))
}

/// The most recent outcome and reason the agent itself reported for attempt `number` of task
/// `id`, with [`crate::report`]. `None` when it reported nothing.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_report(
    journal: &impl Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<(Outcome, Option<String>)>, JournalError> {
    read_and_query(journal, |state| state.report_of(id, number))
}

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

/// Use case: records that the gate named `step` refused to let task `id`'s attempt begin, with
/// `reason` — the run's own words for what failed and what is expected. The task is left
/// `pending`; no attempt is begun for it.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn record_gate_failure(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    step: &str,
    reason: &str,
) -> Result<(), JournalError> {
    let at = clock.now();
    let step = step.to_owned();
    let reason = reason.to_owned();
    decide_and_append(journal, move |_state| {
        Ok::<(Vec<Event>, ()), JournalError>((
            vec![Event::GateFailed {
                id,
                step: step.clone(),
                reason: reason.clone(),
                at,
            }],
            (),
        ))
    })
}

/// The step name and reason of the most recent gate stop recorded for task `id`. `None` when
/// it was never stopped by a gate, or a later attempt has since begun for it.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn gate_stop_of(
    journal: &dyn Journal,
    id: TaskId,
) -> Result<Option<(String, String)>, JournalError> {
    read_and_query(journal, |state| state.gate_stop_of(id))
}

/// Use case: starts the next attempt at the pending task numbered `id`, marking it running.
/// Returns the attempt's number, starting at 1 and never reused for this task.
///
/// # Errors
///
/// Fails, recording nothing, when there is no such task, when it is not pending, or when the
/// journal cannot be read or written.
pub(crate) fn begin_attempt(
    journal: &dyn Journal,
    clock: &dyn Clock,
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
    journal: &dyn Journal,
    clock: &dyn Clock,
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
    journal: &dyn Journal,
    clock: &dyn Clock,
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
    journal: &dyn Journal,
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

/// Use case: begins step `step` of attempt `number` of task `id` — one of the ordered steps a
/// task's attempt runs through, run in order and stopping at the first that ends badly.
///
/// # Errors
///
/// Fails, recording nothing, when no attempt numbered `number` is running for this task, or
/// when the journal cannot be read or written.
pub(crate) fn begin_step(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    step: &str,
) -> Result<(), RecordReportError> {
    let at = clock.now();
    let step = step.to_owned();
    decide_and_append(journal, move |state| {
        state
            .decide_begin_step(id, number, step.clone(), at)
            .map(|event| (vec![event], ()))
    })
}

/// Use case: ends step `step` of attempt `number` of task `id` with `run`. `reported` is the
/// fine-grained outcome the agent itself reported for this step — distinct from `run.status`,
/// which collapses several outcomes (`failed` and `too-large`, say) into one
/// [`crate::TaskStatus`] — so the step's own display can tell them apart later; `None` for a
/// step the tool records as already passed (the sync and health-check steps), which no agent
/// ever reports an outcome for.
///
/// # Errors
///
/// Fails, recording nothing, when no attempt numbered `number` is running for this task, or
/// when the journal cannot be read or written.
pub(crate) fn end_step(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    step: &str,
    run: AttemptRun<'_>,
    reported: Option<Outcome>,
) -> Result<(), RecordReportError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state
            .decide_end_step(id, number, step, run, reported, at)
            .map(|event| (vec![event], ()))
    })
}

/// The task currently running, and its current attempt's number — a project has at most one at
/// a time. `None` when none is running.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn running(journal: &dyn Journal) -> Result<Option<(TaskId, u32)>, JournalError> {
    read_and_query(journal, crate::queue_state::QueueState::running)
}

/// The most recent attempt at task `id`. `None` when it was never attempted.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_attempt(
    journal: &dyn Journal,
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
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<(Outcome, Option<String>)>, JournalError> {
    read_and_query(journal, |state| state.report_of(id, number))
}

/// The outcome and reason reported while step `step` of attempt `number` of task `id` was
/// open, with [`crate::report`]. `None` when nothing was reported during that step.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn report_of_step(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
    step: &str,
) -> Result<Option<(Outcome, Option<String>)>, JournalError> {
    read_and_query(journal, |state| state.report_of_step(id, number, step))
}

/// The name of the step currently open — begun, not yet ended — for task `id`'s current
/// attempt. `None` when the attempt has no open step: it was never begun through
/// [`begin_step`], or its last step has already ended.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn current_step(
    journal: &dyn Journal,
    id: TaskId,
) -> Result<Option<String>, JournalError> {
    read_and_query(journal, |state| state.current_step(id))
}

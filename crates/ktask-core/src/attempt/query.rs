//! Reading attempts back: everything [`super`] records, read through [`crate::queue_state`]'s
//! own folded state rather than decided or appended.

use std::time::SystemTime;

use crate::queue_state::read_and_query;
use crate::{Attempt, Journal, JournalError, Outcome, TaskId};

/// The step name, reason and when of the most recent gate stop recorded for task `id`. `None`
/// when it was never stopped by a gate, or a later attempt has since begun for it.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn gate_stop_of(
    journal: &dyn Journal,
    id: TaskId,
) -> Result<Option<(String, String, SystemTime)>, JournalError> {
    read_and_query(journal, |state| state.gate_stop_of(id))
}

/// When attempt `number` of task `id` ended, with [`super::end_attempt`]. `None` while it is
/// still open, or for an attempt that was never begun.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn ended_at(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<SystemTime>, JournalError> {
    read_and_query(journal, |state| state.ended_at(id, number))
}

/// The session the provider reported for attempt `number` of task `id`'s implementation step,
/// with [`super::record_session`]. `None` when it reported none, or has not run yet.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_session(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<String>, JournalError> {
    read_and_query(journal, |state| state.session_of(id, number))
}

/// Whether the resolver's `retry` decision for attempt `number` of task `id` asked to resume
/// its own session, with [`super::record_report`]. `false` when it named none, or reported
/// something other than `retry`.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_retry_same_session(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<bool, JournalError> {
    read_and_query(journal, |state| state.retry_same_session_of(id, number))
}

/// Whether the resolver's `retry` decision for attempt `number` of task `id` asked for the
/// working tree to be reset before the task's next attempt begins, with
/// [`super::record_report`]. `false` when it named none, or reported something other than
/// `retry`.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_retry_reset_tree(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<bool, JournalError> {
    read_and_query(journal, |state| state.retry_reset_tree_of(id, number))
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

/// Every attempt ever begun at task `id`, oldest first — every one it was retried past
/// included. Empty when it was never attempted.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn all_attempts(
    journal: &dyn Journal,
    id: TaskId,
) -> Result<Vec<Attempt>, JournalError> {
    read_and_query(journal, |state| state.attempts_of(id))
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

/// The model the resolver named for task `id`'s next attempt, with its `retry` decision for
/// attempt `number`. `None` when it named none, or reported something other than `retry`.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_retry_model(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<String>, JournalError> {
    read_and_query(journal, |state| state.retry_model_of(id, number))
}

/// The minutes the resolver's `retry` decision for attempt `number` of task `id` added to the
/// task's next attempt's time limit. `None` when it added none.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn last_retry_more_time(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<u32>, JournalError> {
    read_and_query(journal, |state| state.retry_more_time_of(id, number))
}

/// Whether a step of attempt `number` of task `id` ended at the attempt time limit.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn ended_at_time_limit(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<bool, JournalError> {
    read_and_query(journal, |state| state.ended_at_time_limit(id, number))
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
/// [`super::begin_step`], or its last step has already ended.
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

/// The answer recorded for attempt `number` of task `id`, with [`crate::answer_task`]. `None`
/// when it was never answered.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn answer_of(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<String>, JournalError> {
    read_and_query(journal, |state| state.answer_of(id, number))
}

/// The reason and when task `id` was marked done by the operator's own hand, with
/// [`crate::done_task`]. `None` when it never was.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn done_mark_of(
    journal: &dyn Journal,
    id: TaskId,
) -> Result<Option<(String, SystemTime)>, JournalError> {
    read_and_query(journal, |state| state.done_mark_of(id))
}

/// `reason`, with `answer`, when there is one, appended as "— answer: …" — the one line
/// `status` and the queue screen show for a blocked attempt once [`crate::answer_task`] has
/// recorded an answer for it, and the one a later attempt's own prompt names the same
/// attempt's earlier outcome with, too.
pub(crate) fn with_answer(reason: Option<String>, answer: Option<&str>) -> Option<String> {
    match (reason, answer) {
        (Some(reason), Some(answer)) => Some(format!("{reason} — answer: {answer}")),
        (reason, _) => reason,
    }
}

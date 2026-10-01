//! Attempts: beginning one, recording what happened to it, marking it running, ending it, and
//! reading it back.
//!
//! The rules live in [`crate::queue_state`], the one place a command is decided against the
//! state its events fold to; this is the thin layer — mirroring [`crate::task`]'s — that reads
//! the journal, decides, and appends, or reads and queries.

use std::time::SystemTime;

use crate::journal::AttemptRun;
use crate::queue_state::decide_and_append;
use crate::{
    BeginAttemptError, Clock, Event, Journal, JournalError, Outcome, RecordReportError, Task,
    TaskDraft, TaskId,
};

mod query;

pub(crate) use query::{
    all_attempts, answer_of, current_step, done_mark_of, gate_stop_of, last_attempt, last_report,
    last_retry_model, last_retry_reset_tree, last_retry_same_session, last_session, report_of_step,
    running, with_answer,
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
    decide_and_append(journal, |state| state.decide_begin_attempt(id, at, None))
}

/// Use case: records `outcome` (and `reason`) for attempt `number` of task `id`, `retry_model`
/// — the model the resolver named for the task's next attempt, when `outcome` is `retry` and it
/// named one — `retry_same_session` — whether the resolver's `retry` decision asked the task's
/// next attempt to resume this one's own session — and `retry_reset_tree` — whether it asked
/// the working tree to be reset before the task's next attempt begins. A later report for the
/// same running attempt is recorded the same way and stands as the current one; both stay in
/// the journal.
///
/// # Errors
///
/// Fails, recording nothing, when no attempt numbered `number` was started for this task, when
/// it was but has since ended, or when the journal cannot be read or written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_report(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    outcome: Outcome,
    reason: Option<&str>,
    retry_model: Option<&str>,
    retry_same_session: bool,
    retry_reset_tree: bool,
) -> Result<(), RecordReportError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state
            .decide_record_report(
                id,
                number,
                outcome,
                reason,
                retry_model,
                retry_same_session,
                retry_reset_tree,
                at,
            )
            .map(|event| (vec![event], ()))
    })
}

/// Use case: records the resolver's `supersede` decision for attempt `number` of task `id`,
/// adding every draft of `drafts`, together, in order, where `id` was. Returns the tasks added.
///
/// # Errors
///
/// Fails, recording nothing, when attempt `number` of task `id` is not the one currently
/// running, or when the journal cannot be read or written.
pub(crate) fn record_supersede(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    drafts: &[TaskDraft],
) -> Result<Vec<Task>, RecordReportError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state.decide_supersede(id, number, drafts, at)
    })
}

/// Use case: records `session` — the session the provider reported — for attempt `number` of
/// task `id`'s implementation step.
///
/// # Errors
///
/// Fails, recording nothing, when no attempt numbered `number` is running for this task, or
/// when the journal cannot be read or written.
pub(crate) fn record_session(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    session: &str,
) -> Result<(), RecordReportError> {
    let at = clock.now();
    let session = session.to_owned();
    decide_and_append(journal, move |state| {
        state
            .decide_record_session(id, number, session.clone(), at)
            .map(|event| (vec![event], ()))
    })
}

/// Use case: begins the next attempt at the pending task numbered `id` and, in the same
/// append, marks it running with `provider` — for a caller (the `run` use case) that always
/// knows its provider up front, so "begun" and "running" are never visible apart, one journal
/// round trip apart from each other. `start_commit` is the project's commit `HEAD` right
/// before this attempt begins, recorded with it so a later attempt's own prompt can show
/// everything the task has changed since its first attempt started. Returns the attempt's
/// number.
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
    start_commit: Option<&str>,
) -> Result<u32, BeginAttemptError> {
    let at = clock.now();
    let provider = provider.to_owned();
    let start_commit = start_commit.map(str::to_owned);
    decide_and_append(journal, move |state| {
        let (mut events, number) = state.decide_begin_attempt(id, at, start_commit.clone())?;
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
    model: Option<&str>,
) -> Result<(), RecordReportError> {
    let at = clock.now();
    let step = step.to_owned();
    let model = model.map(str::to_owned);
    decide_and_append(journal, move |state| {
        state
            .decide_begin_step(id, number, step.clone(), model.clone(), at)
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

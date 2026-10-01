//! Mirroring an appended [`Event`] into the `tasks` cache — mechanical bookkeeping so a
//! task's current status can be read without folding the whole journal. No rule about the
//! queue or its attempts is decided here, only what `ktask_core::queue_state` already
//! decided.

use std::time::SystemTime;

use ktask_core::{Event, TaskDraft, TaskId, TaskStatus};
use rusqlite::Transaction;

use super::encode::to_seconds;

/// Inserts the row [`Event::TaskAdded`] mirrors into the `tasks` cache.
fn mirror_task_added(
    transaction: &Transaction<'_>,
    task_id: i64,
    draft: &TaskDraft,
    at: SystemTime,
) -> Result<(), rusqlite::Error> {
    let criteria = serde_json::json!(draft.criteria).to_string();
    let links = serde_json::json!(draft.links).to_string();
    transaction.execute(
        "INSERT INTO tasks
             (id, order_key, title, body, criteria, kind, links, status, created_at)
         VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            task_id,
            &draft.title,
            &draft.body,
            &criteria,
            draft.kind.as_str(),
            &links,
            TaskStatus::Pending.as_str(),
            to_seconds(at),
        ),
    )?;
    Ok(())
}

/// Sets the `tasks` cache row for `id` to `status`, inside `transaction` — every kind of event
/// that settles a task at one fixed status and nothing else shares this.
fn set_status(
    transaction: &Transaction<'_>,
    id: i64,
    status: TaskStatus,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "UPDATE tasks SET status = ?2 WHERE id = ?1",
        (id, status.as_str()),
    )?;
    Ok(())
}

/// Mirrors `event` into the `tasks` cache, inside `transaction`.
pub(super) fn mirror(transaction: &Transaction<'_>, event: &Event) -> Result<(), rusqlite::Error> {
    let task_id = |id: TaskId| i64::try_from(id.0).unwrap_or(i64::MAX);
    match event {
        Event::TaskAdded { id, draft, at, .. } => {
            mirror_task_added(transaction, task_id(*id), draft, *at)?;
        }
        Event::TaskCancelled { id, .. } => {
            set_status(transaction, task_id(*id), TaskStatus::Cancelled)?;
        }
        Event::AttemptStarted { id, number, .. } => {
            transaction.execute(
                "UPDATE tasks SET status = ?2, attempt_number = ?3 WHERE id = ?1",
                (task_id(*id), TaskStatus::Running.as_str(), *number),
            )?;
        }
        Event::AttemptRunning { .. }
        | Event::AttemptReported { .. }
        | Event::StepStarted { .. }
        | Event::StepEnded { .. }
        | Event::GateFailed { .. } => {
            // Carries no status change of its own: a gate stop leaves the task pending.
        }
        Event::AttemptEnded { id, status, .. } => {
            set_status(transaction, task_id(*id), *status)?;
        }
        Event::TaskRetried { id, .. } | Event::TaskAnswered { id, .. } => {
            set_status(transaction, task_id(*id), TaskStatus::Pending)?;
        }
        Event::TaskDoneByUser { id, .. } => {
            set_status(transaction, task_id(*id), TaskStatus::Done)?;
        }
    }
    Ok(())
}

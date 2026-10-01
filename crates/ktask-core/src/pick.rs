//! Picks the next task a run should attempt: the first in queue order not already `done`.

use crate::run::{Attempted, RunEnd, RunError};
use crate::{Journal, Task, TaskId, TaskKind, TaskStatus, list_tasks};

/// What [`pick_next_task`] found the run should do next.
pub(crate) enum Pick {
    /// Attempt this pending task.
    Task(Task),
    /// Stop: the next pending task is kind `human`.
    Human(TaskId),
    /// Stop: the first task in queue order that is not `done` already ended `failed`,
    /// `blocked` or `failed-unknown` — the run refuses to skip past it.
    Blocked {
        /// The task the run refuses to skip past.
        id: TaskId,
        /// What it ended at.
        status: TaskStatus,
        /// Why, from its last attempt.
        reason: Option<String>,
    },
    /// Stop: nothing is pending — `queue_is_empty` says whether the queue holds no tasks at
    /// all, or holds tasks that are all already decided.
    NothingLeft { queue_is_empty: bool },
}

/// Looks at the queue in order and decides what the run does next: the first task not
/// already `done` or `skipped` — a skipped task is resolved the same as a done one, and never
/// blocks or is attempted again. A `pending` task is attempted (or stops the run, when it is
/// kind `human`); a task that already ended `failed`, `blocked` or `failed-unknown` stops the
/// run without attempting anything, since the queue runs in order and nothing after it may run
/// ahead of it.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn pick_next_task(journal: &impl Journal) -> Result<Pick, RunError> {
    let tasks = list_tasks(journal)?;
    let Some(next) = tasks
        .iter()
        .find(|task| !matches!(task.status, TaskStatus::Done | TaskStatus::Skipped))
    else {
        return Ok(Pick::NothingLeft {
            queue_is_empty: tasks.is_empty(),
        });
    };
    match next.status {
        TaskStatus::Pending => Ok(if next.kind == TaskKind::Human {
            Pick::Human(next.id)
        } else {
            Pick::Task(next.clone())
        }),
        TaskStatus::Failed | TaskStatus::Blocked | TaskStatus::FailedUnknown => {
            let reason = crate::attempt::last_attempt(journal, next.id)?
                .and_then(|attempt| attempt.ended)
                .and_then(|ended| ended.reason);
            Ok(Pick::Blocked {
                id: next.id,
                status: next.status,
                reason,
            })
        }
        TaskStatus::Running | TaskStatus::Cancelled | TaskStatus::Done | TaskStatus::Skipped => {
            unreachable!(
                "a task left running is resolved before this loop runs; cancelled, done and \
                 skipped are filtered out above"
            )
        }
    }
}

/// Why the run ends when nothing is left pending: `Completed` when this run attempted
/// something first, otherwise `EmptyQueue` or `NothingPending` depending on `queue_is_empty`.
pub(crate) fn end_when_nothing_left(attempted: &[Attempted], queue_is_empty: bool) -> RunEnd {
    if !attempted.is_empty() {
        RunEnd::Completed
    } else if queue_is_empty {
        RunEnd::EmptyQueue
    } else {
        RunEnd::NothingPending
    }
}

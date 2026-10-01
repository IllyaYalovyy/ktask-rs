//! Small, pure questions asked of a loaded [`QueueView`]: whether a task can still be removed,
//! is running or cancelled, and which task a fresh view should keep selected.

use ktask_core::{QueueView, TaskId, TaskStatus};

/// Whether `view` shows the task `id` and it can still be removed: a cancelled or a running
/// one cannot.
pub(super) fn removable(view: &QueueView, id: TaskId) -> bool {
    view.tasks
        .iter()
        .any(|task| task.id == id && !cancelled(view, id) && !is_running(view, id))
}

/// Whether `view` shows the task `id` as running.
pub(super) fn is_running(view: &QueueView, id: TaskId) -> bool {
    view.tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Running)
}

/// Whether `view` shows the task `id` as cancelled.
pub(super) fn cancelled(view: &QueueView, id: TaskId) -> bool {
    view.tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Cancelled)
}

/// The task to keep selected once `queue` replaces `previous`, given the selection it had: the
/// same task if it is still there, otherwise the one that took its place in the list, or the
/// last.
pub(super) fn reselect(
    previous: Option<&QueueView>,
    selected: Option<TaskId>,
    queue: &QueueView,
) -> Option<TaskId> {
    let index = match (previous, selected) {
        (Some(_), Some(id)) if queue.tasks.iter().any(|task| task.id == id) => {
            return Some(id);
        }
        (Some(old), Some(id)) => old.tasks.iter().position(|task| task.id == id),
        _ => None,
    };
    let last = queue.tasks.len().checked_sub(1)?;
    queue
        .tasks
        .get(index.unwrap_or(0).min(last))
        .map(|task| task.id)
}

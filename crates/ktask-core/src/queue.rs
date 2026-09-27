//! What the queue screen shows.

use crate::Project;

/// How many tasks are in each status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusSummary {
    /// Tasks waiting their turn.
    pub pending: usize,
    /// Tasks being worked on.
    pub running: usize,
    /// Tasks that finished successfully.
    pub done: usize,
    /// Tasks that ended in failure.
    pub failed: usize,
    /// Tasks that were removed from the queue.
    pub cancelled: usize,
}

/// A project's queue as the queue screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueView {
    /// The project the queue belongs to.
    pub project: Project,
    /// How many tasks are in each status.
    pub summary: StatusSummary,
}

/// Use case: the queue of `project`.
///
/// No task can be added yet, so the queue is always empty.
#[must_use]
pub fn queue_view(project: Project) -> QueueView {
    QueueView {
        project,
        summary: StatusSummary::default(),
    }
}

#[cfg(test)]
mod tests {
    use crate::fakes::project;

    use super::*;

    #[test]
    fn a_new_queue_has_every_count_at_zero() {
        let view = queue_view(project("app", 10));
        assert_eq!(view.project, project("app", 10));
        assert_eq!(
            view.summary,
            StatusSummary {
                pending: 0,
                running: 0,
                done: 0,
                failed: 0,
                cancelled: 0
            }
        );
    }
}

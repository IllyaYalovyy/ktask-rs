//! What the queue screen shows.

use crate::task::without_cancelled;
use crate::{Journal, JournalError, Project, Task, TaskStatus, list_all_tasks};

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

impl StatusSummary {
    /// The counts for `tasks`.
    #[must_use]
    pub fn of(tasks: &[Task]) -> Self {
        let mut summary = Self::default();
        for task in tasks {
            let count = match task.status {
                TaskStatus::Pending => &mut summary.pending,
                TaskStatus::Running => &mut summary.running,
                TaskStatus::Done => &mut summary.done,
                TaskStatus::Failed => &mut summary.failed,
                TaskStatus::Cancelled => &mut summary.cancelled,
            };
            *count += 1;
        }
        summary
    }
}

/// A project's queue as the queue screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueView {
    /// The project the queue belongs to.
    pub project: Project,
    /// How many tasks are in each status.
    pub summary: StatusSummary,
    /// The tasks to show, in queue order: without the cancelled ones, which the summary
    /// counts anyway, unless they were asked for. Positions count what is shown.
    pub tasks: Vec<Task>,
}

/// Use case: the queue of `project`, whose journal is `journal`; with the cancelled tasks in
/// their places when `show_cancelled`.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn queue_view(
    project: Project,
    journal: &impl Journal,
    show_cancelled: bool,
) -> Result<QueueView, JournalError> {
    let tasks = list_all_tasks(journal)?;
    Ok(QueueView {
        project,
        summary: StatusSummary::of(&tasks),
        tasks: if show_cancelled {
            tasks
        } else {
            without_cancelled(tasks)
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft, project};
    use crate::{Placement, TaskStatus, add_task};

    use super::*;

    #[test]
    fn a_new_queue_has_every_count_at_zero_and_no_tasks() {
        let view = queue_view(project("app", 10), &FakeJournal::default(), false).unwrap();
        assert_eq!(view.project, project("app", 10));
        assert_eq!(view.summary, StatusSummary::default());
        assert_eq!(view.tasks, vec![]);
    }

    #[test]
    fn the_view_holds_the_tasks_in_order_and_counts_them_by_status() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(1));
        for title in ["a", "b", "c", "d"] {
            add_task(&journal, &clock, &draft(title), Placement::End).unwrap();
        }
        let statuses = [
            TaskStatus::Done,
            TaskStatus::Pending,
            TaskStatus::Failed,
            TaskStatus::Pending,
        ];
        for (task, status) in journal.tasks.borrow_mut().iter_mut().zip(statuses) {
            task.status = status;
        }
        let view = queue_view(project("app", 10), &journal, false).unwrap();
        let titles: Vec<_> = view.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["a", "b", "c", "d"]);
        assert_eq!(
            view.summary,
            StatusSummary {
                pending: 2,
                running: 0,
                done: 1,
                failed: 1,
                cancelled: 0
            }
        );
    }

    #[test]
    fn removed_tasks_are_counted_as_cancelled_and_not_shown() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(1));
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock, &draft(title), Placement::End).unwrap();
        }
        crate::remove_task(&journal, &clock, crate::TaskId(2)).unwrap();
        let view = queue_view(project("app", 10), &journal, false).unwrap();
        let shown: Vec<_> = view.tasks.iter().map(|t| (t.position, &*t.title)).collect();
        assert_eq!(shown, [(1, "a"), (2, "c")]);
        assert_eq!(
            view.summary,
            StatusSummary {
                pending: 2,
                cancelled: 1,
                ..StatusSummary::default()
            }
        );
    }

    #[test]
    fn asked_for_the_cancelled_tasks_are_shown_in_their_places_and_counted_in_the_positions() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(1));
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock, &draft(title), Placement::End).unwrap();
        }
        crate::remove_task(&journal, &clock, crate::TaskId(2)).unwrap();
        let view = queue_view(project("app", 10), &journal, true).unwrap();
        let shown: Vec<_> = view
            .tasks
            .iter()
            .map(|t| (t.position, &*t.title, t.status))
            .collect();
        assert_eq!(
            shown,
            [
                (1, "a", TaskStatus::Pending),
                (2, "b", TaskStatus::Cancelled),
                (3, "c", TaskStatus::Pending)
            ]
        );
        assert_eq!(view.summary.cancelled, 1);
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            queue_view(project("app", 10), &journal, false),
            Err(failure)
        );
    }
}

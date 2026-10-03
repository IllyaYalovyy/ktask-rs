//! What the queue screen shows.

use std::collections::HashMap;

use std::time::Duration;

use crate::status::{AttemptLine, AttemptOutcome, DoneMark, status_with_output};
use crate::task::without_hidden_statuses;
use crate::{
    AttemptOutput, Clock, Journal, JournalError, Project, RunLock, Task, TaskId, TaskStatus,
    list_all_tasks,
};

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
    /// Tasks blocked on a decision from the operator.
    pub blocked: usize,
    /// Tasks whose attempt ended with no report from the agent, and tasks the journal still
    /// calls running whose run is not alive to finish them — shown `interrupted` in their own
    /// row, but counted here with the other attempts the tool itself had to call, not the
    /// agent.
    pub failed_unknown: usize,
    /// Tasks that were removed from the queue.
    pub cancelled: usize,
    /// Tasks the resolver decided are no longer the right thing to do.
    pub skipped: usize,
    /// Tasks the resolver decided were too large to finish as written, and replaced with
    /// smaller tasks.
    pub superseded: usize,
}

impl StatusSummary {
    /// The counts for `tasks`, `attempts` giving each one that was attempted its shown
    /// outcome — a task shown [`AttemptOutcome::Interrupted`] is never counted `running`; it
    /// joins `failed_unknown`, the count of the other attempts the tool called on its own.
    #[must_use]
    pub fn of(tasks: &[Task], attempts: &HashMap<TaskId, AttemptLine>) -> Self {
        let mut summary = Self::default();
        for task in tasks {
            let interrupted = attempts
                .get(&task.id)
                .is_some_and(|attempt| attempt.outcome == AttemptOutcome::Interrupted);
            let count = if interrupted {
                &mut summary.failed_unknown
            } else {
                match task.status {
                    TaskStatus::Pending => &mut summary.pending,
                    TaskStatus::Running => &mut summary.running,
                    TaskStatus::Done => &mut summary.done,
                    TaskStatus::Failed => &mut summary.failed,
                    TaskStatus::Blocked => &mut summary.blocked,
                    TaskStatus::FailedUnknown => &mut summary.failed_unknown,
                    TaskStatus::Cancelled => &mut summary.cancelled,
                    TaskStatus::Skipped => &mut summary.skipped,
                    TaskStatus::Superseded => &mut summary.superseded,
                }
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
    /// The tasks to show, in queue order: without the cancelled, skipped or superseded ones,
    /// which the summary counts anyway, unless they were asked for. Positions count what is
    /// shown.
    pub tasks: Vec<Task>,
    /// The most recent attempt of every task that has one — the same line `status` shows,
    /// from the same use case. A task with no entry here was never attempted.
    pub attempts: HashMap<TaskId, AttemptLine>,
    /// Every earlier attempt of every task that has one, oldest first — every one it was
    /// retried past, from the same use case `status` reads its own history from. A task on
    /// its first attempt, or never attempted, has no entry here.
    pub history: HashMap<TaskId, Vec<AttemptLine>>,
    /// The reason and when, for every task sealed `done` by the operator's own hand, with
    /// [`crate::done_task`], from the same use case `status` reads it from. A task not marked
    /// done this way has no entry here.
    pub done_by_user: HashMap<TaskId, DoneMark>,
}

/// Use case: the queue of `project`, whose journal is `journal`; with the cancelled, skipped and
/// superseded tasks in their places when `show_cancelled`.
///
/// A task the journal still calls `running` is shown `running` only while `lock` says a run is
/// actually alive; otherwise it is shown `interrupted` at once, with no need to wait for the
/// next `run` to reconcile it — the same rule `status` follows, from the same use case.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when `lock` cannot be used.
pub fn queue_view(
    project: Project,
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    show_cancelled: bool,
) -> Result<QueueView, JournalError> {
    queue_view_with_output(
        project,
        journal,
        clock,
        lock,
        &crate::NoAttemptOutput,
        Duration::MAX,
        show_cancelled,
    )
}

/// Use case: the queue with the live output state for its running provider attempt. This is
/// the view both interfaces use when they can read the project's append-only output files.
///
/// # Errors
///
/// Returns a journal error when the queue cannot be read, or its run lock cannot be checked.
pub fn queue_view_with_output(
    project: Project,
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: &impl AttemptOutput,
    silent_after: Duration,
    show_cancelled: bool,
) -> Result<QueueView, JournalError> {
    let tasks = list_all_tasks(journal)?;
    let entries = status_with_output(journal, clock, lock, output, silent_after)?;
    let attempts: HashMap<TaskId, AttemptLine> = entries
        .iter()
        .map(|entry| (entry.task, entry.attempt.clone()))
        .collect();
    let done_by_user = done_marks(journal, &tasks)?;
    let history: HashMap<TaskId, Vec<AttemptLine>> = entries
        .into_iter()
        .filter(|entry| !entry.history.is_empty())
        .map(|entry| (entry.task, entry.history))
        .collect();
    Ok(QueueView {
        project,
        summary: StatusSummary::of(&tasks, &attempts),
        tasks: if show_cancelled {
            tasks
        } else {
            without_hidden_statuses(tasks)
        },
        attempts,
        history,
        done_by_user,
    })
}

/// The manual-done record for each task that has one. `status` has an entry only for a task
/// with an attempt or a gate stop, so a task marked done before its first run must be read here
/// instead.
fn done_marks(
    journal: &impl Journal,
    tasks: &[Task],
) -> Result<HashMap<TaskId, DoneMark>, JournalError> {
    tasks
        .iter()
        .filter_map(|task| {
            crate::attempt::done_mark_of(journal, task.id)
                .transpose()
                .map(|mark| mark.map(|(reason, at)| (task.id, DoneMark { reason, at })))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::fakes::{FakeClock, FakeJournal, FakeRunLock, at, draft, project};
    use crate::{AttemptRun, Placement, TaskId, TaskStatus, add_task, status};

    use super::*;

    /// A lock no run holds — irrelevant whenever nothing is running.
    fn no_run() -> FakeRunLock {
        FakeRunLock::free()
    }

    #[test]
    fn a_new_queue_has_every_count_at_zero_and_no_tasks() {
        let view = queue_view(
            project("app", 10),
            &FakeJournal::default(),
            &FakeClock(at(0)),
            &no_run(),
            false,
        )
        .unwrap();
        assert_eq!(view.project, project("app", 10));
        assert_eq!(view.summary, StatusSummary::default());
        assert_eq!(view.tasks, vec![]);
        assert!(view.attempts.is_empty());
    }

    #[test]
    fn the_view_holds_the_tasks_in_order_and_counts_them_by_status() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(1));
        for title in ["a", "b", "c", "d"] {
            add_task(&journal, &clock, &draft(title), Placement::End).unwrap();
        }
        // `a` and `c` run an attempt that ends at the status wanted; `b` and `d` stay pending.
        for (id, status) in [
            (TaskId(1), TaskStatus::Done),
            (TaskId(3), TaskStatus::Failed),
        ] {
            let number = crate::attempt::begin_attempt(&journal, &clock, id).unwrap();
            crate::attempt::end_attempt(
                &journal,
                id,
                number,
                AttemptRun {
                    duration: Duration::ZERO,
                    exit_code: Some(0),
                    status,
                    reason: None,
                },
                clock.0,
            )
            .unwrap();
        }
        let view = queue_view(project("app", 10), &journal, &clock, &no_run(), false).unwrap();
        let titles: Vec<_> = view.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["a", "b", "c", "d"]);
        assert_eq!(
            view.summary,
            StatusSummary {
                pending: 2,
                done: 1,
                failed: 1,
                ..StatusSummary::default()
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
        crate::remove_task(&journal, &clock, TaskId(2)).unwrap();
        let view = queue_view(project("app", 10), &journal, &clock, &no_run(), false).unwrap();
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
        crate::remove_task(&journal, &clock, TaskId(2)).unwrap();
        let view = queue_view(project("app", 10), &journal, &clock, &no_run(), true).unwrap();
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
    fn a_skipped_task_is_counted_and_hidden_the_same_way_a_cancelled_one_is() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(1));
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock, &draft(title), Placement::End).unwrap();
        }
        let number = crate::attempt::begin_attempt(&journal, &clock, TaskId(2)).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(2),
            number,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Skipped,
                reason: Some("no longer relevant"),
            },
            clock.0,
        )
        .unwrap();

        let hidden = queue_view(project("app", 10), &journal, &clock, &no_run(), false).unwrap();
        let shown: Vec<_> = hidden
            .tasks
            .iter()
            .map(|t| (t.position, &*t.title))
            .collect();
        assert_eq!(shown, [(1, "a"), (2, "c")]);
        assert_eq!(
            hidden.summary,
            StatusSummary {
                pending: 2,
                skipped: 1,
                ..StatusSummary::default()
            }
        );

        let shown_view = queue_view(project("app", 10), &journal, &clock, &no_run(), true).unwrap();
        let rows: Vec<_> = shown_view
            .tasks
            .iter()
            .map(|t| (t.position, &*t.title, t.status))
            .collect();
        assert_eq!(
            rows,
            [
                (1, "a", TaskStatus::Pending),
                (2, "b", TaskStatus::Skipped),
                (3, "c", TaskStatus::Pending)
            ]
        );
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            queue_view(
                project("app", 10),
                &journal,
                &FakeClock(at(0)),
                &no_run(),
                false
            ),
            Err(failure)
        );
    }

    #[test]
    fn a_task_with_an_attempt_carries_the_same_attempt_line_status_would_show() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock, TaskId(1), "echo", None).unwrap();

        let view = queue_view(
            project("app", 10),
            &journal,
            &FakeClock(at(30)),
            &no_run(),
            false,
        )
        .unwrap();

        let expected = status(&journal, &FakeClock(at(30)), &no_run()).unwrap()[0]
            .attempt
            .clone();
        assert_eq!(view.attempts.get(&TaskId(1)), Some(&expected));
    }

    #[test]
    fn a_task_left_running_with_no_run_alive_is_shown_interrupted_and_counted_with_failed_unknown()
    {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock, TaskId(1), "echo", None).unwrap();

        let view = queue_view(project("app", 10), &journal, &clock, &no_run(), false).unwrap();

        // The task's own persisted status is untouched, but it is shown `interrupted`, not
        // `running`, and counted with the other attempts the tool itself had to end, not the
        // agent.
        assert_eq!(view.tasks[0].status, TaskStatus::Running);
        assert_eq!(
            view.attempts.get(&TaskId(1)).unwrap().outcome,
            AttemptOutcome::Interrupted
        );
        assert_eq!(
            view.summary,
            StatusSummary {
                failed_unknown: 1,
                ..StatusSummary::default()
            }
        );
    }

    #[test]
    fn a_task_marked_done_by_the_user_carries_the_reason_and_when() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        let number = crate::attempt::begin_attempt(&journal, &clock, TaskId(1)).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            number,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(1),
                status: TaskStatus::Failed,
                reason: Some("it broke"),
            },
            clock.0,
        )
        .unwrap();
        crate::done_task(&journal, &FakeClock(at(5)), TaskId(1), "fixed by hand").unwrap();

        let view = queue_view(project("app", 10), &journal, &clock, &no_run(), false).unwrap();

        assert_eq!(view.tasks[0].status, TaskStatus::Done);
        assert_eq!(
            view.done_by_user.get(&TaskId(1)),
            Some(&DoneMark {
                reason: "fixed by hand".to_owned(),
                at: at(5),
            })
        );
    }

    #[test]
    fn a_task_marked_done_before_its_first_attempt_carries_the_reason_and_when() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        crate::done_task(
            &journal,
            &FakeClock(at(5)),
            TaskId(1),
            "finished before its run",
        )
        .unwrap();

        let view = queue_view(project("app", 10), &journal, &clock, &no_run(), false).unwrap();

        assert_eq!(view.tasks[0].status, TaskStatus::Done);
        assert_eq!(
            view.done_by_user.get(&TaskId(1)),
            Some(&DoneMark {
                reason: "finished before its run".to_owned(),
                at: at(5),
            })
        );
    }
}

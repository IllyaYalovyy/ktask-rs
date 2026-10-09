//! The task use cases: add one or many, remove one, and list them — with or without the
//! cancelled and skipped ones.

use crate::queue_state::{QueueState, decide_and_append};
use crate::{
    AcknowledgeError, AnswerError, CancelError, Clock, DoneError, Journal, JournalError, RetryError,
};

use super::validate::{AddError, draft_problems};
use super::{Placement, Task, TaskDraft, TaskId, TaskStatus};

/// Use case: adds every task of `drafts`, together, to the queue at `placement`, in order.
///
/// The journal records one event per task and places them together, with the next numbers.
/// No other task's number changes.
///
/// # Errors
///
/// Fails, adding nothing, when `placement` names a task that does not exist or was
/// cancelled, or when the journal cannot be read or written. Does not check `drafts`
/// themselves: the caller has done that.
pub(crate) fn add_tasks(
    journal: &impl Journal,
    clock: &impl Clock,
    drafts: &[TaskDraft],
    placement: Placement,
) -> Result<Vec<Task>, AddError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state
            .decide_add(drafts, placement, at)
            .map_err(AddError::from)
    })
}

/// Use case: adds the task `draft` to the queue at `placement`. When the task is not added,
/// every reason is given, not only the first, so that all of them can be put right at once.
///
/// The journal records one event and numbers and places the task in the same transaction.
/// No other task's number changes.
///
/// # Errors
///
/// Fails, adding nothing, with every rule `draft` breaks — a blank title, no criterion or a
/// blank one, a malformed link — in the order of its fields, or with the one reason
/// `placement` or the journal gives.
pub fn add_task(
    journal: &impl Journal,
    clock: &impl Clock,
    draft: &TaskDraft,
    placement: Placement,
) -> Result<Task, Vec<AddError>> {
    let problems = draft_problems(draft);
    if !problems.is_empty() {
        return Err(problems);
    }
    let added = add_tasks(journal, clock, std::slice::from_ref(draft), placement)
        .map_err(|error| vec![error])?;
    added.into_iter().next().ok_or_else(|| {
        vec![AddError::Journal(JournalError::new(
            "the journal stored no task",
        ))]
    })
}

/// Use case: removes the task numbered `id` from the queue.
///
/// The task is cancelled, not deleted: it stays in the journal, keeps its number, and is
/// shown only when cancelled tasks are asked for.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task, when it is running, when it is
/// cancelled already, or when the journal cannot be written.
pub fn remove_task(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
) -> Result<(), CancelError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state.decide_cancel(id, at).map(|event| (vec![event], ()))
    })
}

/// Use case: sends the task numbered `id` back to `pending`, after it ended `failed`,
/// `failed-unknown` or `blocked`, so the next run picks it up again. Every attempt already
/// recorded for it stays in the journal, and the next one begins at the next number; the
/// working tree is left exactly as the task's attempts so far have left it.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task, or its status is not `failed`,
/// `failed-unknown` or `blocked` — `pending`, `running`, `done` and `cancelled` are all
/// refused, naming the task's own status.
pub fn retry_task(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
) -> Result<(), RetryError> {
    let at = clock.now();
    decide_and_append(journal, |state| {
        state.decide_retry(id, at).map(|event| (vec![event], ()))
    })
}

/// Use case: records `text` as the answer to the question task `id`'s attempt asked, and
/// sends it back to `pending`, so the next run picks it up with it: the next attempt's own
/// prompt carries both the question and this answer. Every attempt already recorded for it
/// stays in the journal, and the next one begins at the next number.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task, its status is not `blocked`, or
/// `text` is empty or only whitespace.
pub fn answer_task(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    text: &str,
) -> Result<(), AnswerError> {
    if text.trim().is_empty() {
        return Err(AnswerError::EmptyAnswer);
    }
    let at = clock.now();
    decide_and_append(journal, |state| {
        state
            .decide_answer(id, text, at)
            .map(|event| (vec![event], ()))
    })
}

/// Use case: marks the task numbered `id` `done`, by the operator's own hand, with `reason`,
/// whenever the operator says its work is finished outside the tool. Every attempt already
/// recorded for it stays in the journal; the next run continues past it, the same as any other
/// task already `done`.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task or `reason` is empty or only whitespace.
pub fn done_task(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    reason: &str,
) -> Result<(), DoneError> {
    if reason.trim().is_empty() {
        return Err(DoneError::EmptyReason);
    }
    let at = clock.now();
    decide_and_append(journal, |state| {
        state
            .decide_done(id, reason, at)
            .map(|event| (vec![event], ()))
    })
}

/// Use case: acknowledges the pending human task numbered `id`, marking it `done` without
/// starting an agent attempt. `message`, when present and not blank, is recorded with the
/// acknowledgement so the operator's action remains auditable.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task or it is not a pending human task.
pub fn acknowledge_task(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    message: Option<&str>,
) -> Result<(), AcknowledgeError> {
    let at = clock.now();
    let message = message.filter(|text| !text.trim().is_empty());
    decide_and_append(journal, |state| {
        state
            .decide_acknowledge(id, message, at)
            .map(|event| (vec![event], ()))
    })
}

/// Every task, cancelled and skipped ones included, in queue order, with its full status:
/// pending, cancelled or skipped, or what its most recent attempt, if it has one, is at —
/// `running`, or what it ended at.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn list_all_tasks(journal: &(impl Journal + ?Sized)) -> Result<Vec<Task>, JournalError> {
    let events = journal.events()?;
    Ok(QueueState::fold(&events).into_tasks())
}

/// Use case: every task in the queue, in order, without the cancelled, skipped or superseded
/// ones. Positions count the tasks shown.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn list_tasks(journal: &(impl Journal + ?Sized)) -> Result<Vec<Task>, JournalError> {
    Ok(without_hidden_statuses(list_all_tasks(journal)?))
}

/// `tasks` without the cancelled, skipped or superseded ones, positions counting from 1 again.
pub(crate) fn without_hidden_statuses(tasks: Vec<Task>) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|task| {
            !matches!(
                task.status,
                TaskStatus::Cancelled | TaskStatus::Skipped | TaskStatus::Superseded
            )
        })
        .enumerate()
        .map(|(index, task)| Task {
            position: index + 1,
            ..task
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{DoneError, JournalError, TaskKind};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(500))
    }

    #[test]
    fn an_added_task_is_pending_numbered_from_one_and_kept_as_written() {
        let journal = FakeJournal::default();
        let written = TaskDraft {
            title: "Title".to_owned(),
            body: "Body\nmore".to_owned(),
            criteria: vec!["first".to_owned(), "second".to_owned()],
            kind: TaskKind::Human,
            links: vec![
                "github:owner/repo#12".to_owned(),
                "https://example.com/x".to_owned(),
            ],
            provider: None,
            model: None,
        };
        let task = add_task(&journal, &clock(), &written, Placement::End).unwrap();
        assert_eq!(
            task,
            Task {
                id: TaskId(1),
                position: 1,
                title: "Title".to_owned(),
                body: "Body\nmore".to_owned(),
                criteria: written.criteria,
                kind: TaskKind::Human,
                links: written.links,
                provider: None,
                model: None,
                status: TaskStatus::Pending,
                created_at: at(500),
            }
        );
        assert_eq!(list_tasks(&journal), Ok(vec![task]));
    }

    #[test]
    fn adding_gives_every_rule_the_draft_breaks_and_adds_nothing() {
        let journal = FakeJournal::default();
        let broken = TaskDraft {
            title: "  ".to_owned(),
            criteria: vec![String::new()],
            links: vec!["nope".to_owned()],
            ..draft("x")
        };
        assert_eq!(
            add_task(&journal, &clock(), &broken, Placement::End),
            Err(vec![
                AddError::EmptyTitle,
                AddError::EmptyCriterion,
                AddError::MalformedLink("nope".to_owned()),
            ])
        );
        let none = TaskDraft {
            criteria: vec![],
            ..draft("")
        };
        assert_eq!(
            add_task(&journal, &clock(), &none, Placement::End),
            Err(vec![AddError::EmptyTitle, AddError::NoCriteria])
        );
        assert_eq!(list_tasks(&journal), Ok(vec![]));
    }

    #[test]
    fn adding_a_valid_draft_at_an_unknown_placement_reports_the_bad_placement() {
        let journal = FakeJournal::default();
        let added = add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        assert_eq!(
            (added.id, added.position, added.title.as_str()),
            (TaskId(1), 1, "a")
        );
        assert_eq!(
            add_task(
                &journal,
                &clock(),
                &draft("b"),
                Placement::Before(TaskId(9))
            ),
            Err(vec![AddError::UnknownTask(TaskId(9))])
        );
        assert_eq!(list_tasks(&journal), Ok(vec![added]));
    }

    #[test]
    fn tasks_are_appended_at_the_end_with_the_next_number() {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        let listed = list_tasks(&journal).unwrap();
        let shown: Vec<_> = listed
            .iter()
            .map(|t| (t.position, t.id, t.title.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (1, TaskId(1), "a"),
                (2, TaskId(2), "b"),
                (3, TaskId(3), "c")
            ]
        );
    }

    /// The titles of the queue, in order, after adding `a`, `b` and `c` and then `new` at
    /// `placement`.
    fn titles_after_inserting(placement: Placement) -> Vec<String> {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        add_task(&journal, &clock(), &draft("new"), placement).unwrap();
        list_tasks(&journal)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect()
    }

    #[test]
    fn a_task_goes_immediately_before_or_after_the_one_named() {
        let before = |id| titles_after_inserting(Placement::Before(TaskId(id)));
        let after = |id| titles_after_inserting(Placement::After(TaskId(id)));
        assert_eq!(before(1), ["new", "a", "b", "c"]);
        assert_eq!(before(2), ["a", "new", "b", "c"]);
        assert_eq!(before(3), ["a", "b", "new", "c"]);
        assert_eq!(after(1), ["a", "new", "b", "c"]);
        assert_eq!(after(2), ["a", "b", "new", "c"]);
        assert_eq!(after(3), ["a", "b", "c", "new"]);
    }

    #[test]
    fn an_inserted_task_gets_the_next_number_and_no_other_number_changes() {
        let journal = FakeJournal::default();
        for title in ["a", "b"] {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        let inserted = add_task(
            &journal,
            &clock(),
            &draft("new"),
            Placement::Before(TaskId(1)),
        )
        .unwrap();
        assert_eq!((inserted.id, inserted.position), (TaskId(3), 1));
        let shown: Vec<_> = list_tasks(&journal)
            .unwrap()
            .iter()
            .map(|t| (t.position, t.id))
            .collect();
        assert_eq!(shown, [(1, TaskId(3)), (2, TaskId(1)), (3, TaskId(2))]);
    }

    #[test]
    fn placing_a_task_next_to_one_that_is_missing_or_cancelled_adds_nothing() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        remove_task(&journal, &clock(), TaskId(1)).unwrap();
        add_task(&journal, &clock(), &draft("b"), Placement::End).unwrap();
        let before = list_tasks(&journal).unwrap();
        for (placement, expected) in [
            (
                Placement::Before(TaskId(9)),
                AddError::UnknownTask(TaskId(9)),
            ),
            (
                Placement::After(TaskId(9)),
                AddError::UnknownTask(TaskId(9)),
            ),
            (
                Placement::Before(TaskId(1)),
                AddError::CancelledTask(TaskId(1)),
            ),
            (
                Placement::After(TaskId(1)),
                AddError::CancelledTask(TaskId(1)),
            ),
        ] {
            assert_eq!(
                add_task(&journal, &clock(), &draft("x"), placement),
                Err(vec![expected])
            );
            assert_eq!(list_tasks(&journal).unwrap(), before);
        }
    }

    fn queue_of_abc_with_b_removed() -> FakeJournal {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        remove_task(&journal, &clock(), TaskId(2)).unwrap();
        journal
    }

    #[test]
    fn a_removed_task_is_left_out_of_the_list_and_the_others_are_renumbered_by_position() {
        let journal = queue_of_abc_with_b_removed();
        let shown: Vec<_> = list_tasks(&journal)
            .unwrap()
            .iter()
            .map(|t| (t.position, t.id, t.status))
            .collect();
        assert_eq!(
            shown,
            [
                (1, TaskId(1), TaskStatus::Pending),
                (2, TaskId(3), TaskStatus::Pending)
            ]
        );
    }

    #[test]
    fn a_removed_task_stays_in_the_full_list_cancelled_and_in_its_place() {
        let journal = queue_of_abc_with_b_removed();
        let shown: Vec<_> = list_all_tasks(&journal)
            .unwrap()
            .iter()
            .map(|t| (t.position, t.id, t.status))
            .collect();
        assert_eq!(
            shown,
            [
                (1, TaskId(1), TaskStatus::Pending),
                (2, TaskId(2), TaskStatus::Cancelled),
                (3, TaskId(3), TaskStatus::Pending)
            ]
        );
    }

    #[test]
    fn a_removed_tasks_number_is_not_reused() {
        let journal = queue_of_abc_with_b_removed();
        let added = add_task(&journal, &clock(), &draft("d"), Placement::End).unwrap();
        assert_eq!(added.id, TaskId(4));
    }

    #[test]
    fn removing_an_unknown_or_a_cancelled_task_is_refused_and_changes_nothing() {
        let journal = queue_of_abc_with_b_removed();
        let before = list_all_tasks(&journal).unwrap();
        assert_eq!(
            remove_task(&journal, &clock(), TaskId(9)),
            Err(CancelError::UnknownTask(TaskId(9)))
        );
        assert_eq!(
            remove_task(&journal, &clock(), TaskId(2)),
            Err(CancelError::AlreadyCancelled(TaskId(2)))
        );
        assert_eq!(list_all_tasks(&journal).unwrap(), before);
    }

    #[test]
    fn removing_a_running_task_is_refused_and_changes_nothing() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
        let before = list_all_tasks(&journal).unwrap();

        assert_eq!(
            remove_task(&journal, &clock(), TaskId(1)),
            Err(CancelError::Running(TaskId(1)))
        );

        assert_eq!(list_all_tasks(&journal).unwrap(), before);
    }

    #[test]
    fn a_journal_failure_is_passed_on_when_removing() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            remove_task(&journal, &clock(), TaskId(1)),
            Err(CancelError::Journal(failure))
        );
    }

    #[test]
    fn an_empty_queue_lists_nothing() {
        assert_eq!(list_tasks(&FakeJournal::default()), Ok(vec![]));
    }

    /// Ends task 1's attempt `number` at `status`, with `reason` when it needs one.
    fn end_at(journal: &FakeJournal, number: u32, status: TaskStatus, reason: Option<&str>) {
        crate::attempt::end_attempt(
            journal,
            TaskId(1),
            number,
            crate::journal::AttemptRun {
                duration: std::time::Duration::ZERO,
                exit_code: Some(0),
                status,
                reason,
            },
            clock().now(),
        )
        .unwrap();
    }

    /// A journal with one pending task, `a`, whose one attempt ended at `status` with `reason`.
    fn journal_with_a_task_ended_at(status: TaskStatus, reason: Option<&str>) -> FakeJournal {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
        end_at(&journal, 1, status, reason);
        journal
    }

    #[test]
    fn retrying_a_failed_a_blocked_or_a_failed_unknown_task_sends_it_back_to_pending() {
        for status in [
            TaskStatus::Failed,
            TaskStatus::Blocked,
            TaskStatus::FailedUnknown,
        ] {
            let journal = journal_with_a_task_ended_at(status, Some("why"));
            assert_eq!(retry_task(&journal, &clock(), TaskId(1)), Ok(()));
            assert_eq!(list_tasks(&journal).unwrap()[0].status, TaskStatus::Pending);
        }
    }

    #[test]
    fn retrying_keeps_the_earlier_attempt_and_the_next_one_takes_the_next_number() {
        let journal = journal_with_a_task_ended_at(TaskStatus::Failed, Some("why"));
        retry_task(&journal, &clock(), TaskId(1)).unwrap();

        let attempts = crate::attempt::all_attempts(&journal, TaskId(1)).unwrap();
        assert_eq!(attempts.len(), 1, "the earlier attempt is still there");
        assert_eq!(attempts[0].number, 1);
        assert_eq!(
            attempts[0].ended.as_ref().map(|end| end.status),
            Some(TaskStatus::Failed)
        );

        let number = crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
        assert_eq!(number, 2);
        let attempts = crate::attempt::all_attempts(&journal, TaskId(1)).unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].number, 1);
        assert_eq!(attempts[1].number, 2);
    }

    #[test]
    fn retrying_a_pending_a_running_or_a_done_task_is_refused_naming_its_status() {
        let pending = FakeJournal::default();
        add_task(&pending, &clock(), &draft("a"), Placement::End).unwrap();
        assert_eq!(
            retry_task(&pending, &clock(), TaskId(1)),
            Err(RetryError::NotRetryable {
                id: TaskId(1),
                status: TaskStatus::Pending
            })
        );

        let running = FakeJournal::default();
        add_task(&running, &clock(), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt(&running, &clock(), TaskId(1)).unwrap();
        assert_eq!(
            retry_task(&running, &clock(), TaskId(1)),
            Err(RetryError::NotRetryable {
                id: TaskId(1),
                status: TaskStatus::Running
            })
        );

        let done = journal_with_a_task_ended_at(TaskStatus::Done, None);
        assert_eq!(
            retry_task(&done, &clock(), TaskId(1)),
            Err(RetryError::NotRetryable {
                id: TaskId(1),
                status: TaskStatus::Done
            })
        );
    }

    #[test]
    fn retrying_an_unknown_task_is_refused_and_names_it() {
        let journal = FakeJournal::default();
        assert_eq!(
            retry_task(&journal, &clock(), TaskId(9)),
            Err(RetryError::UnknownTask(TaskId(9)))
        );
    }

    #[test]
    fn a_retried_tasks_cancelled_status_is_refused_too() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        remove_task(&journal, &clock(), TaskId(1)).unwrap();
        assert_eq!(
            retry_task(&journal, &clock(), TaskId(1)),
            Err(RetryError::NotRetryable {
                id: TaskId(1),
                status: TaskStatus::Cancelled
            })
        );
    }

    #[test]
    fn a_journal_failure_is_passed_on_when_retrying() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            retry_task(&journal, &clock(), TaskId(1)),
            Err(RetryError::Journal(failure))
        );
    }

    #[test]
    fn answering_a_blocked_task_records_the_answer_and_sends_it_to_pending() {
        let journal = journal_with_a_task_ended_at(TaskStatus::Blocked, Some("which path?"));
        assert_eq!(
            answer_task(&journal, &clock(), TaskId(1), "the left one"),
            Ok(())
        );
        assert_eq!(list_tasks(&journal).unwrap()[0].status, TaskStatus::Pending);
        assert_eq!(
            crate::attempt::answer_of(&journal, TaskId(1), 1).unwrap(),
            Some("the left one".to_owned())
        );
    }

    #[test]
    fn answering_a_task_that_is_not_blocked_is_refused_naming_its_status_and_changes_nothing() {
        for (journal, status) in [
            (FakeJournal::default(), TaskStatus::Pending),
            (
                journal_with_a_task_ended_at(TaskStatus::Failed, Some("why")),
                TaskStatus::Failed,
            ),
            (
                journal_with_a_task_ended_at(TaskStatus::Done, None),
                TaskStatus::Done,
            ),
        ] {
            if status == TaskStatus::Pending {
                add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
            }
            let before = list_all_tasks(&journal).unwrap();
            assert_eq!(
                answer_task(&journal, &clock(), TaskId(1), "an answer"),
                Err(AnswerError::NotBlocked {
                    id: TaskId(1),
                    status
                })
            );
            assert_eq!(list_all_tasks(&journal).unwrap(), before);
        }
    }

    #[test]
    fn answering_with_an_empty_or_blank_text_is_refused_and_changes_nothing() {
        let journal = journal_with_a_task_ended_at(TaskStatus::Blocked, Some("which path?"));
        let before = list_all_tasks(&journal).unwrap();
        for text in ["", "   ", "\t\n"] {
            assert_eq!(
                answer_task(&journal, &clock(), TaskId(1), text),
                Err(AnswerError::EmptyAnswer)
            );
        }
        assert_eq!(list_all_tasks(&journal).unwrap(), before);
    }

    #[test]
    fn answering_an_unknown_task_is_refused_and_names_it() {
        let journal = FakeJournal::default();
        assert_eq!(
            answer_task(&journal, &clock(), TaskId(9), "an answer"),
            Err(AnswerError::UnknownTask(TaskId(9)))
        );
    }

    #[test]
    fn a_journal_failure_is_passed_on_when_answering() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            answer_task(&journal, &clock(), TaskId(1), "an answer"),
            Err(AnswerError::Journal(failure))
        );
    }

    #[test]
    fn a_task_that_breaks_a_rule_is_refused_and_adds_nothing() {
        let cases = [
            (
                TaskDraft {
                    title: String::new(),
                    ..draft("x")
                },
                vec![AddError::EmptyTitle],
            ),
            (
                TaskDraft {
                    title: " \t".to_owned(),
                    ..draft("x")
                },
                vec![
                    AddError::EmptyTitle,
                    AddError::ControlCharacterInTitle('\t'),
                ],
            ),
            (
                TaskDraft {
                    criteria: vec![],
                    ..draft("x")
                },
                vec![AddError::NoCriteria],
            ),
            (
                TaskDraft {
                    criteria: vec!["ok".to_owned(), "  ".to_owned()],
                    ..draft("x")
                },
                vec![AddError::EmptyCriterion],
            ),
            (
                TaskDraft {
                    links: vec!["https://ok.example".to_owned(), "nonsense".to_owned()],
                    ..draft("x")
                },
                vec![AddError::MalformedLink("nonsense".to_owned())],
            ),
        ];
        for (bad, expected) in cases {
            let journal = FakeJournal::default();
            assert_eq!(
                add_task(&journal, &clock(), &bad, Placement::End),
                Err(expected)
            );
            assert_eq!(list_tasks(&journal), Ok(vec![]));
        }
    }

    #[test]
    fn marking_any_task_done_seals_it_done() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Blocked,
            TaskStatus::FailedUnknown,
            TaskStatus::Cancelled,
            TaskStatus::Skipped,
            TaskStatus::Superseded,
        ] {
            let journal = if status == TaskStatus::Pending {
                let journal = FakeJournal::default();
                add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
                journal
            } else if status == TaskStatus::Running {
                let journal = FakeJournal::default();
                add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
                crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
                journal
            } else if status == TaskStatus::Cancelled {
                let journal = FakeJournal::default();
                add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
                remove_task(&journal, &clock(), TaskId(1)).unwrap();
                journal
            } else {
                journal_with_a_task_ended_at(status, Some("why"))
            };
            assert_eq!(
                done_task(&journal, &clock(), TaskId(1), "finished by hand"),
                Ok(())
            );
            assert_eq!(
                list_all_tasks(&journal).unwrap()[0].status,
                TaskStatus::Done
            );
        }
    }

    #[test]
    fn marking_done_records_the_reason_and_when() {
        let journal = journal_with_a_task_ended_at(TaskStatus::Failed, Some("why"));
        done_task(&journal, &clock(), TaskId(1), "finished by hand").unwrap();
        assert_eq!(
            crate::attempt::done_mark_of(&journal, TaskId(1)).unwrap(),
            Some(("finished by hand".to_owned(), clock().now()))
        );
    }

    #[test]
    fn acknowledging_a_pending_human_task_marks_it_done_and_records_its_message() {
        let journal = FakeJournal::default();
        let human = TaskDraft {
            kind: TaskKind::Human,
            ..draft("approve the design")
        };
        add_task(&journal, &clock(), &human, Placement::End).unwrap();

        acknowledge_task(&journal, &clock(), TaskId(1), Some("approved")).unwrap();

        assert_eq!(list_tasks(&journal).unwrap()[0].status, TaskStatus::Done);
        assert_eq!(
            journal.events().unwrap().last(),
            Some(&crate::Event::TaskAcknowledged {
                id: TaskId(1),
                message: Some("approved".to_owned()),
                at: clock().now(),
            })
        );
    }

    #[test]
    fn acknowledgement_refuses_an_agent_or_a_human_task_that_is_no_longer_pending() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("agent"), Placement::End).unwrap();
        let human = TaskDraft {
            kind: TaskKind::Human,
            ..draft("human")
        };
        add_task(&journal, &clock(), &human, Placement::End).unwrap();
        acknowledge_task(&journal, &clock(), TaskId(2), None).unwrap();

        assert_eq!(
            acknowledge_task(&journal, &clock(), TaskId(1), None),
            Err(AcknowledgeError::NotAcknowledgeable {
                id: TaskId(1),
                kind: TaskKind::Agent,
                status: TaskStatus::Pending,
            })
        );
        assert_eq!(
            acknowledge_task(&journal, &clock(), TaskId(2), None),
            Err(AcknowledgeError::NotAcknowledgeable {
                id: TaskId(2),
                kind: TaskKind::Human,
                status: TaskStatus::Done,
            })
        );
    }

    #[test]
    fn marking_an_unknown_task_done_is_refused_and_names_it() {
        let journal = FakeJournal::default();
        assert_eq!(
            done_task(&journal, &clock(), TaskId(9), "finished by hand"),
            Err(DoneError::UnknownTask(TaskId(9)))
        );
    }

    #[test]
    fn marking_done_with_an_empty_or_blank_reason_is_refused_and_changes_nothing() {
        let journal = journal_with_a_task_ended_at(TaskStatus::Failed, Some("why"));
        let before = list_all_tasks(&journal).unwrap();
        for reason in ["", "   ", "\t\n"] {
            assert_eq!(
                done_task(&journal, &clock(), TaskId(1), reason),
                Err(DoneError::EmptyReason)
            );
        }
        assert_eq!(list_all_tasks(&journal).unwrap(), before);
    }

    #[test]
    fn a_journal_failure_is_passed_on_when_marking_done() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            done_task(&journal, &clock(), TaskId(1), "finished by hand"),
            Err(DoneError::Journal(failure))
        );
    }

    #[test]
    fn a_title_a_criterion_or_a_link_with_a_control_character_is_refused_and_adds_nothing() {
        let cases = [
            (
                TaskDraft {
                    title: "a\nb".to_owned(),
                    ..draft("x")
                },
                AddError::ControlCharacterInTitle('\n'),
            ),
            (
                TaskDraft {
                    title: "a\tb".to_owned(),
                    ..draft("x")
                },
                AddError::ControlCharacterInTitle('\t'),
            ),
            (
                TaskDraft {
                    title: "a\x1bb".to_owned(),
                    ..draft("x")
                },
                AddError::ControlCharacterInTitle('\x1b'),
            ),
            (
                TaskDraft {
                    criteria: vec!["a\nb".to_owned()],
                    ..draft("x")
                },
                AddError::ControlCharacterInCriterion('\n'),
            ),
            (
                TaskDraft {
                    criteria: vec!["a\tb".to_owned()],
                    ..draft("x")
                },
                AddError::ControlCharacterInCriterion('\t'),
            ),
            (
                TaskDraft {
                    criteria: vec!["a\x1bb".to_owned()],
                    ..draft("x")
                },
                AddError::ControlCharacterInCriterion('\x1b'),
            ),
            (
                TaskDraft {
                    links: vec!["https://example.com/a\nb".to_owned()],
                    ..draft("x")
                },
                AddError::ControlCharacterInLink('\n'),
            ),
            (
                TaskDraft {
                    links: vec!["https://example.com/a\tb".to_owned()],
                    ..draft("x")
                },
                AddError::ControlCharacterInLink('\t'),
            ),
            (
                TaskDraft {
                    links: vec!["https://example.com/a\x1bb".to_owned()],
                    ..draft("x")
                },
                AddError::ControlCharacterInLink('\x1b'),
            ),
        ];
        for (bad, expected) in cases {
            let journal = FakeJournal::default();
            assert_eq!(
                add_task(&journal, &clock(), &bad, Placement::End),
                Err(vec![expected.clone()]),
                "{expected:?}"
            );
            assert_eq!(list_tasks(&journal), Ok(vec![]));
        }
    }

    #[test]
    fn a_body_may_hold_newlines_and_tabs_but_no_other_control_character() {
        let journal = FakeJournal::default();
        let fine = TaskDraft {
            body: "one\ntwo\tthree".to_owned(),
            ..draft("x")
        };
        assert!(add_task(&journal, &clock(), &fine, Placement::End).is_ok());

        let journal = FakeJournal::default();
        let broken = TaskDraft {
            body: "one\x1btwo".to_owned(),
            ..draft("x")
        };
        assert_eq!(
            add_task(&journal, &clock(), &broken, Placement::End),
            Err(vec![AddError::ControlCharacterInBody('\x1b')])
        );
        assert_eq!(list_tasks(&journal), Ok(vec![]));
    }

    #[test]
    fn adding_reports_every_control_character_problem_alongside_the_others() {
        let journal = FakeJournal::default();
        let broken = TaskDraft {
            title: "a\nb".to_owned(),
            body: "fine\nbut\x1bnot this".to_owned(),
            criteria: vec!["ok".to_owned(), "bad\tone".to_owned()],
            links: vec!["https://ok.example".to_owned(), "https://x\x1by".to_owned()],
            ..draft("x")
        };
        assert_eq!(
            add_task(&journal, &clock(), &broken, Placement::End),
            Err(vec![
                AddError::ControlCharacterInTitle('\n'),
                AddError::ControlCharacterInBody('\x1b'),
                AddError::ControlCharacterInCriterion('\t'),
                AddError::ControlCharacterInLink('\x1b'),
            ])
        );
        assert_eq!(list_tasks(&journal), Ok(vec![]));
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            add_task(&journal, &clock(), &draft("x"), Placement::End),
            Err(vec![AddError::Journal(failure.clone())])
        );
        assert_eq!(list_tasks(&journal), Err(failure));
    }
}

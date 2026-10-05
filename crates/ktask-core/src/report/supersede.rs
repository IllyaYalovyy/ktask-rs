//! The `supersede` decision: an agent in the resolve role replacing a task too large to finish
//! as written with smaller ones, placed where it was — pulled out of [`super`] so that module
//! stays within the workspace's file-length limit.

use crate::{Clock, Journal, RecordReportError, Task, TaskId};

use super::{AttemptToken, Outcome, ReportError, outcomes_for_step};

/// Refuses `outcome` for the attempt `token` names when it does not belong to the step
/// currently running for it — shared by [`super::report_impl`] and [`report_supersede`], which
/// cannot go through `report_impl` since `supersede` needs more than one event recorded for it.
///
/// # Errors
///
/// Fails when `outcome` does not belong to the step currently running, or the journal cannot
/// be read.
pub(super) fn check_outcome_for_step(
    journal: &impl Journal,
    token: &AttemptToken,
    outcome: Outcome,
) -> Result<(), ReportError> {
    if let Some(step) =
        crate::attempt::current_step(journal, token.task).map_err(RecordReportError::from)?
        && let Some(expected) = outcomes_for_step(&step)
        && !expected.contains(&outcome)
    {
        return Err(ReportError::WrongStep {
            outcome,
            step,
            expected: expected.to_vec(),
        });
    }
    Ok(())
}

/// The result of a successful `supersede`: the new tasks added, in order, and how many
/// cancelled tasks the file left out, the same as [`crate::Import`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supersede {
    /// The tasks added, in order, replacing the superseded one where it was.
    pub tasks: Vec<Task>,
    /// How many cancelled tasks were left out.
    pub skipped_cancelled: usize,
}

impl Supersede {
    /// The message naming `original`, the task replaced, and how many tasks replaced it with
    /// their IDs, in order.
    #[must_use]
    pub fn message(&self, original: TaskId) -> String {
        let ids = self
            .tasks
            .iter()
            .map(|task| task.id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        match self.tasks.len() {
            1 => format!("task {original} superseded by 1 task: {ids}"),
            n => format!("task {original} superseded by {n} tasks: {ids}"),
        }
    }

    /// The message naming how many cancelled tasks were skipped, or `None` when there were
    /// none.
    #[must_use]
    pub fn skipped_message(&self) -> Option<String> {
        match self.skipped_cancelled {
            0 => None,
            1 => Some("1 cancelled task was skipped".to_owned()),
            n => Some(format!("{n} cancelled tasks were skipped")),
        }
    }
}

/// Use case: records the resolver's `supersede` decision for the attempt `token` names, reading
/// `tasks_json` — a JSON array of tasks in the same format [`crate::import_tasks`] takes — and
/// adding every task it describes, together, in order, where the superseded task was.
///
/// # Errors
///
/// Fails, recording nothing, when `tasks_json` is refused the same way `import_tasks` refuses
/// one — not a JSON array, or any task breaks a rule or carries a field that is neither
/// authored nor tool-managed; when `supersede` does not belong to the step currently running
/// for this attempt — only the resolve step accepts it; or when the journal reports the
/// attempt is unknown or has ended.
pub fn report_supersede(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    tasks_json: &str,
) -> Result<Supersede, ReportError> {
    let (drafts, skipped_cancelled) = crate::import::parse_items(tasks_json)?;
    check_outcome_for_step(journal, token, Outcome::Supersede)?;
    let tasks =
        crate::attempt::record_supersede(journal, clock, token.task, token.number, &drafts)?;
    Ok(Supersede {
        tasks,
        skipped_cancelled,
    })
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{ImportError, Placement, TaskId, TaskStatus, add_task};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(1_000))
    }

    /// A journal with one pending task, begun, running, with its resolve step open — the state
    /// `report_supersede` sees while the resolver is mid-step.
    fn journal_with_the_resolve_step_running() -> FakeJournal {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("too large"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(), TaskId(1), "test", None).unwrap();
        crate::attempt::begin_step(
            &journal,
            &clock(),
            TaskId(1),
            1,
            crate::RESOLVE_STEP,
            None,
            None,
        )
        .unwrap();
        journal
    }

    const THREE: &str = r#"[
        {"title": "one", "criteria": ["a"]},
        {"title": "two", "criteria": ["b"]},
        {"title": "three", "criteria": ["c"]}
    ]"#;

    #[test]
    fn three_tasks_replace_the_original_where_it_was_naming_it_in_the_attempts_reason() {
        let journal = journal_with_the_resolve_step_running();
        let token = AttemptToken::new("proj", TaskId(1), 1);

        let supersede = report_supersede(&journal, &clock(), &token, THREE).unwrap();

        assert_eq!(supersede.skipped_cancelled, 0);
        let ids: Vec<_> = supersede.tasks.iter().map(|task| task.id).collect();
        assert_eq!(ids, [TaskId(2), TaskId(3), TaskId(4)]);
        let titles: Vec<_> = crate::list_all_tasks(&journal)
            .unwrap()
            .into_iter()
            .map(|task| (task.position, task.id, task.title, task.status))
            .collect();
        assert_eq!(
            titles,
            [
                (1, TaskId(1), "too large".to_owned(), TaskStatus::Running),
                (2, TaskId(2), "one".to_owned(), TaskStatus::Pending),
                (3, TaskId(3), "two".to_owned(), TaskStatus::Pending),
                (4, TaskId(4), "three".to_owned(), TaskStatus::Pending),
            ]
        );
        assert_eq!(
            crate::attempt::last_report(&journal, TaskId(1), 1).unwrap(),
            Some((
                Outcome::Supersede,
                Some("superseded by 3 tasks: 2, 3, 4".to_owned())
            ))
        );
    }

    #[test]
    fn a_cancelled_task_in_the_file_is_left_out_and_counted() {
        let journal = journal_with_the_resolve_step_running();
        let token = AttemptToken::new("proj", TaskId(1), 1);
        let json = r#"[
            {"title": "kept", "criteria": ["c"]},
            {"title": "gone", "criteria": ["c"], "status": "cancelled"}
        ]"#;

        let supersede = report_supersede(&journal, &clock(), &token, json).unwrap();

        assert_eq!(supersede.skipped_cancelled, 1);
        assert_eq!(
            supersede.skipped_message().as_deref(),
            Some("1 cancelled task was skipped")
        );
        assert_eq!(supersede.tasks.len(), 1);
    }

    #[test]
    fn an_invalid_file_is_refused_the_same_way_import_is_and_changes_nothing() {
        let journal = journal_with_the_resolve_step_running();
        let token = AttemptToken::new("proj", TaskId(1), 1);
        let before = crate::list_all_tasks(&journal).unwrap();

        let malformed = report_supersede(&journal, &clock(), &token, "[\n  {\"title\": }\n]");
        assert!(
            matches!(
                malformed,
                Err(ReportError::Import(ImportError::Malformed(_)))
            ),
            "{malformed:?}"
        );

        let not_an_array = report_supersede(&journal, &clock(), &token, "{}");
        assert_eq!(
            not_an_array,
            Err(ReportError::Import(ImportError::NotAnArray))
        );

        let invalid = report_supersede(&journal, &clock(), &token, r#"[{"title": "  "}]"#);
        assert!(
            matches!(invalid, Err(ReportError::Import(ImportError::Invalid(_)))),
            "{invalid:?}"
        );

        assert_eq!(crate::list_all_tasks(&journal).unwrap(), before);
        assert_eq!(
            crate::attempt::last_report(&journal, TaskId(1), 1).unwrap(),
            None
        );
    }

    #[test]
    fn supersede_outside_the_resolve_step_is_refused_naming_it() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("too large"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(), TaskId(1), "test", None).unwrap();
        crate::attempt::begin_step(
            &journal,
            &clock(),
            TaskId(1),
            1,
            crate::IMPLEMENTATION,
            None,
            None,
        )
        .unwrap();
        let token = AttemptToken::new("proj", TaskId(1), 1);

        let error = report_supersede(&journal, &clock(), &token, THREE).unwrap_err();

        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Supersede,
                step: crate::IMPLEMENTATION.to_owned(),
                expected: vec![
                    Outcome::Done,
                    Outcome::Failed,
                    Outcome::NeedsInput,
                    Outcome::TooLarge,
                ],
            }
        );
        assert_eq!(crate::list_all_tasks(&journal).unwrap().len(), 1);
    }

    /// A task with `id` and nothing else that matters, for messages that only name IDs.
    fn task_with_id(id: u64) -> Task {
        Task {
            id: TaskId(id),
            position: 0,
            title: "t".to_owned(),
            body: String::new(),
            criteria: vec!["c".to_owned()],
            kind: crate::TaskKind::default(),
            links: vec![],
            status: TaskStatus::Pending,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn message_names_the_original_and_the_new_tasks_by_id() {
        let one = Supersede {
            tasks: vec![task_with_id(6)],
            skipped_cancelled: 0,
        };
        assert_eq!(one.message(TaskId(5)), "task 5 superseded by 1 task: 6");
        let many = Supersede {
            tasks: vec![task_with_id(6), task_with_id(7), task_with_id(8)],
            skipped_cancelled: 0,
        };
        assert_eq!(
            many.message(TaskId(5)),
            "task 5 superseded by 3 tasks: 6, 7, 8"
        );
    }

    #[test]
    fn skipped_message_says_how_many_were_skipped_or_says_nothing() {
        let none = Supersede {
            tasks: vec![],
            skipped_cancelled: 0,
        };
        assert_eq!(none.skipped_message(), None);
        let many = Supersede {
            tasks: vec![],
            skipped_cancelled: 3,
        };
        assert_eq!(
            many.skipped_message().as_deref(),
            Some("3 cancelled tasks were skipped")
        );
    }
}

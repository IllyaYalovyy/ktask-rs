//! Importing tasks from JSON: reading the array, validating every task, adding them all.

use std::error::Error;
use std::fmt;

use serde::Deserialize;

use crate::task::{add_tasks, draft_problems};
use crate::{AddError, Clock, Journal, Placement, Task, TaskDraft, TaskKind, TaskStatus};

/// A task as the JSON array writes it: the authored fields of `list --json`, plus the
/// tool-managed fields it also prints — `id`, `position`, `status` and `created_at` — read
/// only so a list exported from one project imports into another unchanged; their values are
/// otherwise ignored, except that `status` tells a cancelled task apart from the rest. Every
/// field may be left out; what a task needs is checked afterwards, and reported in the same
/// words as for a task added any other way. A field that is neither of these is refused,
/// naming it, by `deny_unknown_fields`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Item {
    title: String,
    body: String,
    criteria: Vec<String>,
    kind: Option<String>,
    links: Vec<String>,
    id: Option<serde_json::Value>,
    position: Option<serde_json::Value>,
    status: Option<serde_json::Value>,
    created_at: Option<serde_json::Value>,
}

/// What one element of the JSON array describes.
enum Parsed {
    /// A task to add.
    Draft(TaskDraft),
    /// A task left out because it was cancelled where it came from.
    Cancelled,
}

/// The result of a successful import: the tasks added, in order, and how many cancelled
/// tasks were left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The tasks added, in order.
    pub tasks: Vec<Task>,
    /// How many cancelled tasks were left out.
    pub skipped_cancelled: usize,
}

impl Import {
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

/// One task of the imported array that cannot be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTask {
    /// Its place in the array, counting from 1.
    pub index: usize,
    /// Everything wrong with it.
    pub problems: Vec<String>,
}

/// Why no task was imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// The text is not JSON; the message says where it stops making sense.
    Malformed(String),
    /// The text is JSON but not an array.
    NotAnArray,
    /// Some tasks cannot be added; every one is listed.
    Invalid(Vec<InvalidTask>),
    /// The tasks could not be added.
    Add(AddError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => write!(f, "not valid JSON: {message}"),
            Self::NotAnArray => f.write_str("expected a JSON array of tasks"),
            Self::Invalid(tasks) => {
                write!(
                    f,
                    "{} invalid, so nothing was imported",
                    match tasks.len() {
                        1 => "1 task is".to_owned(),
                        n => format!("{n} tasks are"),
                    }
                )?;
                tasks.iter().try_for_each(|task| {
                    task.problems
                        .iter()
                        .try_for_each(|problem| write!(f, "\n  - task {}: {problem}", task.index))
                })
            }
            Self::Add(error) => error.fmt(f),
        }
    }
}

impl Error for ImportError {}

/// What `value` describes — a task to add, or one to leave out because it is cancelled — or
/// everything wrong with it.
fn read_item(value: serde_json::Value) -> Result<Parsed, Vec<String>> {
    let item: Item = serde_json::from_value(value).map_err(|e| vec![e.to_string()])?;
    if matches!(&item.status, Some(serde_json::Value::String(s)) if s == TaskStatus::Cancelled.as_str())
    {
        return Ok(Parsed::Cancelled);
    }
    let mut problems = Vec::new();
    let kind = match item.kind.as_deref() {
        None => TaskKind::default(),
        Some(text) => text.parse().unwrap_or_else(|message| {
            problems.push(message);
            TaskKind::default()
        }),
    };
    let draft = TaskDraft {
        title: item.title,
        body: item.body,
        criteria: item.criteria,
        kind,
        links: item.links,
    };
    problems.extend(draft_problems(&draft).iter().map(ToString::to_string));
    if problems.is_empty() {
        Ok(Parsed::Draft(draft))
    } else {
        Err(problems)
    }
}

/// Use case: adds every task of the JSON array `json` to the queue, in order and together, at
/// `placement`, so that what one project's `list --json` or `list --all --json` prints
/// imports into another as it is.
///
/// Each element has the authored fields `list --json` prints — `title`, `body`, `criteria`,
/// `kind`, `links` — plus, when present, the tool-managed ones it also prints — `id`,
/// `position`, `status`, `created_at` — which are ignored, except that a task whose `status`
/// is `cancelled` is left out rather than added; [`Import::skipped_message`] says how many
/// were.
///
/// # Errors
///
/// Fails, adding nothing, when `json` is not a JSON array, when any task breaks a rule or
/// carries a field that is neither authored nor tool-managed — all of them are listed, by
/// their place in the array — when `placement` names a task that does not exist or was
/// cancelled, or when the journal cannot be written.
pub fn import_tasks(
    journal: &impl Journal,
    clock: &impl Clock,
    json: &str,
    placement: Placement,
) -> Result<Import, ImportError> {
    let serde_json::Value::Array(values) =
        serde_json::from_str(json).map_err(|e| ImportError::Malformed(e.to_string()))?
    else {
        return Err(ImportError::NotAnArray);
    };
    let mut drafts = Vec::new();
    let mut invalid = Vec::new();
    let mut skipped_cancelled = 0usize;
    for (index, value) in values.into_iter().enumerate() {
        match read_item(value) {
            Ok(Parsed::Draft(draft)) => drafts.push(draft),
            Ok(Parsed::Cancelled) => skipped_cancelled += 1,
            Err(problems) => invalid.push(InvalidTask {
                index: index + 1,
                problems,
            }),
        }
    }
    if !invalid.is_empty() {
        return Err(ImportError::Invalid(invalid));
    }
    let tasks = add_tasks(journal, clock, &drafts, placement).map_err(ImportError::Add)?;
    Ok(Import {
        tasks,
        skipped_cancelled,
    })
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{JournalError, TaskId, add_task, list_tasks};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(500))
    }

    fn titles(journal: &FakeJournal) -> Vec<String> {
        list_tasks(journal)
            .unwrap()
            .into_iter()
            .map(|task| task.title)
            .collect()
    }

    fn queue_of(titles: &[&str]) -> FakeJournal {
        let journal = FakeJournal::default();
        for title in titles {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        journal
    }

    const THREE: &str = r#"[
        {"title": "one", "criteria": ["a"]},
        {"title": "two", "body": "more\ntext", "criteria": ["a", "b"], "kind": "human",
         "links": ["github:o/r#1", "https://example.com"]},
        {"title": "three", "criteria": ["c"]}
    ]"#;

    #[test]
    fn every_task_is_added_in_order_as_written_with_defaults_for_what_is_left_out() {
        let journal = FakeJournal::default();
        let import = import_tasks(&journal, &clock(), THREE, Placement::End).unwrap();
        let tasks = import.tasks;
        assert_eq!(import.skipped_cancelled, 0);
        assert_eq!(
            tasks.iter().map(|t| (t.id, t.position)).collect::<Vec<_>>(),
            [(TaskId(1), 1), (TaskId(2), 2), (TaskId(3), 3)]
        );
        assert_eq!(list_tasks(&journal).unwrap(), tasks);
        assert_eq!(tasks[0].kind, TaskKind::Agent);
        assert_eq!(tasks[0].body, "");
        assert!(tasks[0].links.is_empty());
        assert_eq!(tasks[1].kind, TaskKind::Human);
        assert_eq!(tasks[1].body, "more\ntext");
        assert_eq!(tasks[1].criteria, ["a", "b"]);
        assert_eq!(tasks[1].links, ["github:o/r#1", "https://example.com"]);
    }

    #[test]
    fn the_batch_goes_together_before_or_after_the_task_named() {
        let batch = r#"[{"title":"x","criteria":["c"]},{"title":"y","criteria":["c"]}]"#;
        for (placement, expected) in [
            (Placement::End, ["a", "b", "c", "x", "y"]),
            (Placement::Before(TaskId(1)), ["x", "y", "a", "b", "c"]),
            (Placement::Before(TaskId(2)), ["a", "x", "y", "b", "c"]),
            (Placement::After(TaskId(2)), ["a", "b", "x", "y", "c"]),
            (Placement::After(TaskId(3)), ["a", "b", "c", "x", "y"]),
        ] {
            let journal = queue_of(&["a", "b", "c"]);
            import_tasks(&journal, &clock(), batch, placement).unwrap();
            assert_eq!(titles(&journal), expected, "{placement:?}");
        }
    }

    #[test]
    fn an_empty_array_imports_nothing() {
        let journal = queue_of(&["a"]);
        assert_eq!(
            import_tasks(&journal, &clock(), " [ ] ", Placement::End),
            Ok(Import {
                tasks: vec![],
                skipped_cancelled: 0
            })
        );
        assert_eq!(titles(&journal), ["a"]);
    }

    #[test]
    fn every_invalid_task_is_named_by_its_place_and_problem_and_nothing_is_added() {
        let journal = queue_of(&["a"]);
        let json = r#"[
            {"title": "fine", "criteria": ["c"]},
            {"title": " ", "criteria": []},
            {"title": "kind", "criteria": ["c"], "kind": "robot", "links": ["nonsense"]},
            {"title": "extra", "criteria": ["c"], "assignee": "bob"},
            {"title": 5, "criteria": ["c"]},
            "text"
        ]"#;
        let Err(ImportError::Invalid(invalid)) =
            import_tasks(&journal, &clock(), json, Placement::End)
        else {
            panic!("expected invalid tasks");
        };
        let shown: Vec<_> = invalid
            .iter()
            .map(|task| (task.index, task.problems.len()))
            .collect();
        assert_eq!(shown, [(2, 2), (3, 2), (4, 1), (5, 1), (6, 1)]);
        assert!(invalid[0].problems[0].contains("title is empty"));
        assert!(invalid[0].problems[1].contains("criterion"));
        assert!(invalid[1].problems[0].contains("robot"));
        assert!(invalid[1].problems[1].contains("nonsense"));
        assert!(invalid[2].problems[0].contains("assignee"));
        assert!(invalid[3].problems[0].contains("string"));
        assert_eq!(titles(&journal), ["a"]);
    }

    #[test]
    fn tool_managed_fields_are_read_and_ignored_so_a_full_listing_imports_unchanged() {
        let journal = FakeJournal::default();
        let json = r#"[
            {"id": 7, "position": 1, "title": "one", "body": "", "criteria": ["a"],
             "kind": "agent", "links": [], "status": "done", "created_at": "2024-01-01T00:00:00Z"},
            {"id": 8, "position": 2, "title": "two", "body": "", "criteria": ["b"],
             "kind": "human", "links": [], "status": "pending", "created_at": "2024-01-02T00:00:00Z"}
        ]"#;
        let import = import_tasks(&journal, &clock(), json, Placement::End).unwrap();
        assert_eq!(import.skipped_cancelled, 0);
        assert_eq!(
            import.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [TaskId(1), TaskId(2)]
        );
        assert_eq!(titles(&journal), ["one", "two"]);
    }

    #[test]
    fn a_cancelled_task_is_skipped_and_the_others_are_added() {
        let journal = FakeJournal::default();
        let json = r#"[
            {"title": "kept one", "criteria": ["c"]},
            {"title": "gone", "criteria": ["c"], "status": "cancelled"},
            {"title": "kept two", "criteria": ["c"]}
        ]"#;
        let import = import_tasks(&journal, &clock(), json, Placement::End).unwrap();
        assert_eq!(import.skipped_cancelled, 1);
        assert_eq!(
            import.skipped_message().as_deref(),
            Some("1 cancelled task was skipped")
        );
        assert_eq!(titles(&journal), ["kept one", "kept two"]);
    }

    #[test]
    fn skipped_message_says_how_many_were_skipped_or_says_nothing() {
        let none = Import {
            tasks: vec![],
            skipped_cancelled: 0,
        };
        assert_eq!(none.skipped_message(), None);
        let one = Import {
            tasks: vec![],
            skipped_cancelled: 1,
        };
        assert_eq!(
            one.skipped_message().as_deref(),
            Some("1 cancelled task was skipped")
        );
        let many = Import {
            tasks: vec![],
            skipped_cancelled: 3,
        };
        assert_eq!(
            many.skipped_message().as_deref(),
            Some("3 cancelled tasks were skipped")
        );
    }

    #[test]
    fn the_message_lists_each_problem_under_its_task() {
        let error = ImportError::Invalid(vec![InvalidTask {
            index: 2,
            problems: vec!["first".to_owned(), "second".to_owned()],
        }]);
        assert_eq!(
            error.to_string(),
            "1 task is invalid, so nothing was imported\n  - task 2: first\n  - task 2: second"
        );
    }

    #[test]
    fn text_that_is_not_json_or_not_an_array_adds_nothing() {
        let journal = FakeJournal::default();
        let Err(ImportError::Malformed(message)) =
            import_tasks(&journal, &clock(), "[\n  {\"title\": }\n]", Placement::End)
        else {
            panic!("expected malformed JSON");
        };
        assert!(message.contains("line 2 column"), "{message}");
        for json in ["{}", "\"x\"", "3", "null"] {
            assert_eq!(
                import_tasks(&journal, &clock(), json, Placement::End),
                Err(ImportError::NotAnArray)
            );
        }
        assert_eq!(titles(&journal), Vec::<String>::new());
    }

    #[test]
    fn a_place_that_does_not_exist_or_a_journal_failure_adds_nothing() {
        let journal = queue_of(&["a"]);
        assert_eq!(
            import_tasks(&journal, &clock(), THREE, Placement::After(TaskId(9))),
            Err(ImportError::Add(AddError::UnknownTask(TaskId(9))))
        );
        assert_eq!(titles(&journal), ["a"]);
        let failure = JournalError::new("disk on fire");
        assert_eq!(
            import_tasks(
                &FakeJournal::failing(failure.clone()),
                &clock(),
                THREE,
                Placement::End
            ),
            Err(ImportError::Add(AddError::Journal(failure)))
        );
    }
}

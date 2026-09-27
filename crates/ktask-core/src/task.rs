//! Tasks: what the queue holds, and the rules for what may be put in it.

use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use crate::{AppendError, CancelError, Clock, Journal, JournalError};

/// The number a task is known by: assigned once, in order, never reused, never changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who does a task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskKind {
    /// An agent works on it.
    #[default]
    Agent,
    /// A person does it.
    Human,
}

impl TaskKind {
    /// The name the kind is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
        }
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskKind {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "agent" => Ok(Self::Agent),
            "human" => Ok(Self::Human),
            _ => Err(format!("unknown kind {text:?}: expected agent or human")),
        }
    }
}

/// Where a task is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Waiting its turn.
    Pending,
    /// Being worked on.
    Running,
    /// Finished successfully.
    Done,
    /// Ended in failure.
    Failed,
    /// Removed from the queue.
    Cancelled,
}

impl TaskStatus {
    /// The name the status is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Self::Pending,
            Self::Running,
            Self::Done,
            Self::Failed,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
        .ok_or_else(|| format!("unknown status {text:?}"))
    }
}

/// A task in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The task's number.
    pub id: TaskId,
    /// Its place in the queue, counting from 1.
    pub position: usize,
    /// One line saying what the task is.
    pub title: String,
    /// The longer description; empty when there is none.
    pub body: String,
    /// What must be true for the task to be done; never empty.
    pub criteria: Vec<String>,
    /// Who does it.
    pub kind: TaskKind,
    /// References to related work, each `github:owner/repo#NUMBER` or an `http(s)` URL.
    pub links: Vec<String>,
    /// Where it is in its life.
    pub status: TaskStatus,
    /// When it was added.
    pub created_at: SystemTime,
}

/// What a task is made of before the queue has numbered and placed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDraft {
    /// One line saying what the task is.
    pub title: String,
    /// The longer description.
    pub body: String,
    /// What must be true for the task to be done.
    pub criteria: Vec<String>,
    /// Who does it.
    pub kind: TaskKind,
    /// References to related work.
    pub links: Vec<String>,
}

/// Where a new task goes in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// After every other task.
    End,
    /// Immediately before this task.
    Before(TaskId),
    /// Immediately after this task.
    After(TaskId),
}

impl Placement {
    /// Where the task that follows one just placed goes, when a batch is placed here: after
    /// `previous`, so the batch keeps its order — or still at the end.
    #[must_use]
    pub fn then_after(self, previous: TaskId) -> Self {
        match self {
            Self::End => Self::End,
            Self::Before(_) | Self::After(_) => Self::After(previous),
        }
    }
}

/// Why a task was not added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// The title is empty or only whitespace.
    EmptyTitle,
    /// There is no acceptance criterion.
    NoCriteria,
    /// An acceptance criterion is empty or only whitespace.
    EmptyCriterion,
    /// A link is neither a `github:owner/repo#NUMBER` reference nor an `http(s)` URL.
    MalformedLink(String),
    /// The task the new one was to be placed next to does not exist.
    UnknownTask(TaskId),
    /// The task the new one was to be placed next to was cancelled.
    CancelledTask(TaskId),
    /// The journal could not be used.
    Journal(JournalError),
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => f.write_str("the title is empty: a task needs a title"),
            Self::NoCriteria => f.write_str("a task needs at least one acceptance criterion"),
            Self::EmptyCriterion => f.write_str("an acceptance criterion is empty"),
            Self::MalformedLink(link) => write!(
                f,
                "malformed link {link:?}: expected github:owner/repo#NUMBER or an http(s) URL"
            ),
            Self::UnknownTask(id) => write!(f, "there is no task {id}"),
            Self::CancelledTask(id) => write!(f, "task {id} is cancelled"),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AddError {}

impl From<AppendError> for AddError {
    fn from(error: AppendError) -> Self {
        match error {
            AppendError::UnknownTask(id) => Self::UnknownTask(id),
            AppendError::CancelledTask(id) => Self::CancelledTask(id),
            AppendError::Journal(error) => Self::Journal(error),
        }
    }
}

/// Whether `link` is a `github:owner/repo#NUMBER` reference or an `http(s)` URL.
fn is_link(link: &str) -> bool {
    let name = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if let Some(reference) = link.strip_prefix("github:") {
        let Some((repository, number)) = reference.split_once('#') else {
            return false;
        };
        let Some((owner, repo)) = repository.split_once('/') else {
            return false;
        };
        return name(owner)
            && name(repo)
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit());
    }
    let rest = link
        .strip_prefix("https://")
        .or_else(|| link.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        !rest.is_empty() && !rest.starts_with('/') && !rest.chars().any(char::is_whitespace)
    })
}

/// Every rule `draft` breaks, in the order of its fields; empty when it may be added.
pub(crate) fn draft_problems(draft: &TaskDraft) -> Vec<AddError> {
    let mut problems = Vec::new();
    if draft.title.trim().is_empty() {
        problems.push(AddError::EmptyTitle);
    }
    if draft.criteria.is_empty() {
        problems.push(AddError::NoCriteria);
    }
    if draft.criteria.iter().any(|c| c.trim().is_empty()) {
        problems.push(AddError::EmptyCriterion);
    }
    problems.extend(
        draft
            .links
            .iter()
            .filter(|link| !is_link(link))
            .map(|link| AddError::MalformedLink(link.clone())),
    );
    problems
}

/// Use case: adds the task `draft` to the queue at `placement`.
///
/// The journal records one event and numbers and places the task in the same transaction.
/// No other task's number changes.
///
/// # Errors
///
/// Fails, adding nothing, when the title is blank, there is no criterion or one is blank, a
/// link is malformed, `placement` names a task that does not exist or was cancelled, or the
/// journal cannot be written.
pub fn add_task(
    journal: &impl Journal,
    clock: &impl Clock,
    draft: &TaskDraft,
    placement: Placement,
) -> Result<Task, AddError> {
    if let Some(problem) = draft_problems(draft).into_iter().next() {
        return Err(problem);
    }
    Ok(journal.append_task(draft, placement, clock.now())?)
}

/// Use case: [`add_task`], for a person filling in a form: when the task is not added, every
/// reason is given, not only the first, so that all of them can be put right at once.
///
/// # Errors
///
/// Fails, adding nothing, with every rule `draft` breaks in the order of its fields, or with
/// the one reason `placement` or the journal gives.
pub fn add_task_listing_problems(
    journal: &impl Journal,
    clock: &impl Clock,
    draft: &TaskDraft,
    placement: Placement,
) -> Result<Task, Vec<AddError>> {
    let problems = draft_problems(draft);
    if !problems.is_empty() {
        return Err(problems);
    }
    add_task(journal, clock, draft, placement).map_err(|error| vec![error])
}

/// Use case: removes the task numbered `id` from the queue.
///
/// The task is cancelled, not deleted: it stays in the journal, keeps its number, and is
/// shown only when cancelled tasks are asked for.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task, when it is cancelled already, or
/// when the journal cannot be written.
pub fn remove_task(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
) -> Result<(), CancelError> {
    journal.cancel_task(id, clock.now())
}

/// Use case: every task in the queue, in order, without the cancelled ones. Positions count
/// the tasks shown.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn list_tasks(journal: &impl Journal) -> Result<Vec<Task>, JournalError> {
    Ok(without_cancelled(journal.tasks()?))
}

/// Use case: every task, cancelled ones included, in queue order. Positions count them all.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn list_all_tasks(journal: &impl Journal) -> Result<Vec<Task>, JournalError> {
    journal.tasks()
}

/// `tasks` without the cancelled ones, positions counting from 1 again.
pub(crate) fn without_cancelled(tasks: Vec<Task>) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|task| task.status != TaskStatus::Cancelled)
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
                status: TaskStatus::Pending,
                created_at: at(500),
            }
        );
        assert_eq!(list_tasks(&journal), Ok(vec![task]));
    }

    #[test]
    fn listing_problems_gives_every_rule_the_draft_breaks_and_adds_nothing() {
        let journal = FakeJournal::default();
        let broken = TaskDraft {
            title: "  ".to_owned(),
            criteria: vec![String::new()],
            links: vec!["nope".to_owned()],
            ..draft("x")
        };
        assert_eq!(
            add_task_listing_problems(&journal, &clock(), &broken, Placement::End),
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
            add_task_listing_problems(&journal, &clock(), &none, Placement::End),
            Err(vec![AddError::EmptyTitle, AddError::NoCriteria])
        );
        assert_eq!(list_tasks(&journal), Ok(vec![]));
    }

    #[test]
    fn listing_problems_adds_a_valid_draft_like_add_task_and_reports_a_bad_placement() {
        let journal = FakeJournal::default();
        let added =
            add_task_listing_problems(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        assert_eq!(
            (added.id, added.position, added.title.as_str()),
            (TaskId(1), 1, "a")
        );
        assert_eq!(
            add_task_listing_problems(
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
        journal.tasks.borrow_mut()[0].status = TaskStatus::Cancelled;
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
                Err(expected)
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

    #[test]
    fn a_task_that_breaks_a_rule_is_refused_and_adds_nothing() {
        let cases = [
            (
                TaskDraft {
                    title: String::new(),
                    ..draft("x")
                },
                AddError::EmptyTitle,
            ),
            (
                TaskDraft {
                    title: " \t".to_owned(),
                    ..draft("x")
                },
                AddError::EmptyTitle,
            ),
            (
                TaskDraft {
                    criteria: vec![],
                    ..draft("x")
                },
                AddError::NoCriteria,
            ),
            (
                TaskDraft {
                    criteria: vec!["ok".to_owned(), "  ".to_owned()],
                    ..draft("x")
                },
                AddError::EmptyCriterion,
            ),
            (
                TaskDraft {
                    links: vec!["https://ok.example".to_owned(), "nonsense".to_owned()],
                    ..draft("x")
                },
                AddError::MalformedLink("nonsense".to_owned()),
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
    fn github_references_and_http_urls_are_links() {
        for link in [
            "github:owner/repo#1",
            "github:my-org/my_repo.rs#123",
            "http://example.com",
            "https://example.com/a/b?c=d#e",
        ] {
            assert!(is_link(link), "{link}");
        }
    }

    #[test]
    fn anything_else_is_not_a_link() {
        for link in [
            "",
            "github:",
            "github:owner/repo",
            "github:owner/repo#",
            "github:owner/repo#x1",
            "github:owner#1",
            "github:/repo#1",
            "github:owner/repo/more#1",
            "github:o wner/repo#1",
            "gitlab:owner/repo#1",
            "ftp://example.com",
            "https://",
            "https:///path",
            "https://exa mple.com",
            "example.com",
        ] {
            assert!(!is_link(link), "{link}");
        }
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            add_task(&journal, &clock(), &draft("x"), Placement::End),
            Err(AddError::Journal(failure.clone()))
        );
        assert_eq!(list_tasks(&journal), Err(failure));
    }

    #[test]
    fn kinds_and_statuses_read_back_as_written() {
        for kind in [TaskKind::Agent, TaskKind::Human] {
            assert_eq!(kind.as_str().parse(), Ok(kind));
        }
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(status.as_str().parse(), Ok(status));
        }
        assert!("robot".parse::<TaskKind>().unwrap_err().contains("robot"));
        assert!("stuck".parse::<TaskStatus>().is_err());
        assert_eq!(TaskKind::default(), TaskKind::Agent);
    }
}

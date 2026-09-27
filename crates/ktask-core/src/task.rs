//! Tasks: what the queue holds, and the rules for what may be put in it.

use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use crate::{Clock, Journal, JournalError};

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
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl Error for AddError {}

impl From<JournalError> for AddError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
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

/// Use case: adds the task `draft` at the end of the queue.
///
/// The journal records one event and numbers the task in the same transaction.
///
/// # Errors
///
/// Fails, adding nothing, when the title is blank, there is no criterion or one is blank, a
/// link is malformed, or the journal cannot be written.
pub fn add_task(
    journal: &impl Journal,
    clock: &impl Clock,
    draft: &TaskDraft,
) -> Result<Task, AddError> {
    if draft.title.trim().is_empty() {
        return Err(AddError::EmptyTitle);
    }
    if draft.criteria.is_empty() {
        return Err(AddError::NoCriteria);
    }
    if draft.criteria.iter().any(|c| c.trim().is_empty()) {
        return Err(AddError::EmptyCriterion);
    }
    if let Some(link) = draft.links.iter().find(|link| !is_link(link)) {
        return Err(AddError::MalformedLink(link.clone()));
    }
    Ok(journal.append_task(draft, clock.now())?)
}

/// Use case: every task in the queue, in order.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn list_tasks(journal: &impl Journal) -> Result<Vec<Task>, JournalError> {
    journal.tasks()
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
        let task = add_task(&journal, &clock(), &written).unwrap();
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
    fn tasks_are_appended_at_the_end_with_the_next_number() {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(), &draft(title)).unwrap();
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
            assert_eq!(add_task(&journal, &clock(), &bad), Err(expected));
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
            add_task(&journal, &clock(), &draft("x")),
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

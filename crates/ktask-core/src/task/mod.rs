//! Tasks: what the queue holds, and the rules for what may be put in it.

use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

mod use_cases;
mod validate;

pub use use_cases::{
    acknowledge_task, add_task, answer_task, done_task, list_all_tasks, list_tasks, remove_task,
    retry_task,
};
pub(crate) use use_cases::{add_tasks, without_hidden_statuses};
pub use validate::AddError;
pub(crate) use validate::draft_problems;
pub use validate::provider_problem;

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
    /// The agent needs a decision from the operator before the task can continue.
    Blocked,
    /// The attempt ended with no report from the agent: a crash, or killed at its time
    /// limit.
    FailedUnknown,
    /// Removed from the queue.
    Cancelled,
    /// The resolver decided the task is no longer the right thing to do: the reason is why.
    Skipped,
    /// The resolver decided the task is too large to finish as written, and replaced it with
    /// smaller tasks placed where it was.
    Superseded,
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
            Self::Blocked => "blocked",
            Self::FailedUnknown => "failed-unknown",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::Superseded => "superseded",
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
            Self::Blocked,
            Self::FailedUnknown,
            Self::Cancelled,
            Self::Skipped,
            Self::Superseded,
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
    /// The provider this task uses for its agent steps, when it overrides the project setting.
    pub provider: Option<String>,
    /// The model this task uses for its agent steps, when it overrides the project setting.
    pub model: Option<String>,
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
    /// The provider this task uses for its agent steps, when it overrides the project setting.
    pub provider: Option<String>,
    /// The model this task uses for its agent steps, when it overrides the project setting.
    pub model: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
            TaskStatus::Blocked,
            TaskStatus::FailedUnknown,
            TaskStatus::Cancelled,
            TaskStatus::Skipped,
            TaskStatus::Superseded,
        ] {
            assert_eq!(status.as_str().parse(), Ok(status));
        }
        assert!("robot".parse::<TaskKind>().unwrap_err().contains("robot"));
        assert!("stuck".parse::<TaskStatus>().is_err());
        assert_eq!(TaskKind::default(), TaskKind::Agent);
    }
}

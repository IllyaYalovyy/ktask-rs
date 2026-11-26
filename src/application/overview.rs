//! Every registered project of this instance at a glance. Read-only and not scoped to one
//! project: the Projects screen and `ktask projects` both list from here.
use super::{Kind, lifecycle::State};
use crate::storage::{Database, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

/// The first unfinished task of a project in execution order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentTask {
    pub id: i64,
    pub title: String,
    pub kind: Kind,
    pub state: State,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub root: PathBuf,
    /// Unfinished tasks, the current one included.
    pub queued: i64,
    pub finished: i64,
    pub current: Option<CurrentTask>,
}

/// What a project asks of its owner, derived from its current task and runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    /// The project root is no longer a directory.
    Missing,
    /// A live runner is working on the current task.
    Running,
    /// The current task waits for the owner: an approval, answer, prerequisite or failure.
    NeedsOwner,
    /// A run was paused, or its runner is gone, and continues on Resume.
    Paused,
    /// Pending work that Run would start.
    Ready,
    /// Nothing queued.
    Idle,
}

impl Attention {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Running => "running",
            Self::NeedsOwner => "needs_owner",
            Self::Paused => "paused",
            Self::Ready => "ready",
            Self::Idle => "idle",
        }
    }
}

impl Project {
    /// `runner_alive`: whether this project's supervisor holds its runner lease.
    pub fn attention(&self, runner_alive: bool) -> Attention {
        if !self.root.is_dir() {
            return Attention::Missing;
        }
        let Some(current) = &self.current else {
            return Attention::Idle;
        };
        match current.state {
            State::NeedsApproval | State::NeedsInput | State::NeedsDependency | State::Failed => {
                Attention::NeedsOwner
            }
            State::Running if runner_alive => Attention::Running,
            State::Running | State::Paused => Attention::Paused,
            // A queue run between two tasks has a pending current and a live runner.
            _ if runner_alive => Attention::Running,
            _ if current.kind == Kind::Human => Attention::NeedsOwner,
            _ => Attention::Ready,
        }
    }

    /// The row's state as the owner reads it.
    pub fn state_label(&self, runner_alive: bool) -> &'static str {
        self.attention(runner_alive)
            .label(self.current.as_ref().map(|task| task.state))
    }
}

impl Attention {
    /// The state as the owner reads it, given the state of the current task.
    pub fn label(self, current: Option<State>) -> &'static str {
        match (self, current) {
            (Self::Missing, _) => "Missing folder",
            (Self::Idle, _) | (_, None) => "Idle",
            (Self::Running, _) => "Running",
            (Self::Ready, _) => "Ready",
            (Self::Paused, Some(State::Running)) => "Runner stopped",
            (Self::NeedsOwner, Some(State::Pending)) => "Your task",
            (_, Some(state)) => state.label(),
        }
    }
}

/// One project as `ktask --json projects` reports it and as a linked child sends it to its
/// parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub id: i64,
    pub name: String,
    pub root: String,
    pub state: Attention,
    pub runner_alive: bool,
    pub queued: i64,
    pub finished: i64,
    pub current: Option<CurrentTask>,
}

impl Summary {
    pub fn of(project: &Project, runner_alive: bool) -> Self {
        Self {
            id: project.id,
            name: project.name.clone(),
            root: project.root.to_string_lossy().into_owned(),
            state: project.attention(runner_alive),
            runner_alive,
            queued: project.queued,
            finished: project.finished,
            current: project.current.clone(),
        }
    }
}

/// The first unfinished task in execution order, a running one first: the rule of the
/// `current_tasks` view, read from the task and attempt tables. The view evaluates that
/// rule once per queued task, which costs seconds across several long queues.
const CURRENT_ID: &str = "coalesce(
    (SELECT t.id FROM tasks t JOIN attempts a ON a.task_id = t.id
        WHERE t.project_id = p.id AND t.closed_at_ms IS NULL AND a.state = 'running'
            AND a.number = (SELECT max(number) FROM attempts WHERE task_id = t.id)
        ORDER BY t.order_key LIMIT 1),
    (SELECT t.id FROM tasks t WHERE t.project_id = p.id AND t.closed_at_ms IS NULL
        ORDER BY t.order_key LIMIT 1))";

/// Every registered project by name, with its queue size and current task.
pub fn projects(db: &Database) -> Result<Vec<Project>> {
    let db = db.connection();
    let mut statement = db.prepare(&format!(
        "SELECT p.id, p.name, p.root_path,
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.closed_at_ms IS NULL),
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.closed_at_ms IS NOT NULL),
            {CURRENT_ID}
         FROM projects p ORDER BY p.name COLLATE NOCASE, p.id"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok((
            Project {
                id: row.get(0)?,
                name: row.get(1)?,
                root: PathBuf::from(OsString::from_vec(row.get(2)?)),
                queued: row.get(3)?,
                finished: row.get(4)?,
                current: None,
            },
            row.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut current =
        db.prepare("SELECT title, kind, status, wait_reason FROM task_details WHERE id = ?1")?;
    let mut projects = Vec::new();
    for row in rows {
        let (mut project, id) = row?;
        if let Some(id) = id {
            project.current = Some(current.query_row([id], |row| {
                let kind: Option<String> = row.get(1)?;
                let status: String = row.get(2)?;
                let wait: Option<String> = row.get(3)?;
                Ok(CurrentTask {
                    id,
                    title: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    kind: kind.as_deref().and_then(Kind::parse).unwrap_or(Kind::Ai),
                    state: State::of(&status, wait.as_deref().filter(|_| status == "waiting")),
                })
            })?);
        }
        projects.push(project);
    }
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(current: Option<(Kind, State)>) -> Project {
        Project {
            id: 1,
            name: "fixture".into(),
            // Any directory that exists.
            root: std::env::temp_dir(),
            queued: i64::from(current.is_some()),
            finished: 0,
            current: current.map(|(kind, state)| CurrentTask {
                id: 7,
                title: "Task".into(),
                kind,
                state,
            }),
        }
    }

    #[test]
    fn attention_follows_the_current_task_and_the_runner() {
        let cases = [
            (None, false, Attention::Idle, "Idle"),
            (
                Some((Kind::Ai, State::Pending)),
                false,
                Attention::Ready,
                "Ready",
            ),
            (
                Some((Kind::Ai, State::Pending)),
                true,
                Attention::Running,
                "Running",
            ),
            (
                Some((Kind::Human, State::Pending)),
                false,
                Attention::NeedsOwner,
                "Your task",
            ),
            (
                Some((Kind::Ai, State::Running)),
                true,
                Attention::Running,
                "Running",
            ),
            (
                Some((Kind::Ai, State::Running)),
                false,
                Attention::Paused,
                "Runner stopped",
            ),
            (
                Some((Kind::Ai, State::Paused)),
                false,
                Attention::Paused,
                "Paused",
            ),
            (
                Some((Kind::Ai, State::Failed)),
                false,
                Attention::NeedsOwner,
                "Failed",
            ),
            // A question waits for the owner even while its runner is alive.
            (
                Some((Kind::Ai, State::NeedsInput)),
                true,
                Attention::NeedsOwner,
                "Needs input",
            ),
            (
                Some((Kind::Docs, State::NeedsApproval)),
                false,
                Attention::NeedsOwner,
                "Needs approval",
            ),
            (
                Some((Kind::Ai, State::NeedsDependency)),
                false,
                Attention::NeedsOwner,
                "Needs dependency",
            ),
        ];
        for (current, alive, attention, label) in cases {
            let project = project(current);
            assert_eq!(project.attention(alive), attention, "{current:?} {alive}");
            assert_eq!(project.state_label(alive), label, "{current:?} {alive}");
        }
    }

    #[test]
    fn a_project_without_its_folder_is_missing_whatever_it_queued() {
        let mut project = project(Some((Kind::Ai, State::Failed)));
        project.root = std::env::temp_dir().join("ktask-overview-no-such-folder");
        assert_eq!(project.attention(true), Attention::Missing);
        assert_eq!(project.state_label(true), "Missing folder");
    }
}

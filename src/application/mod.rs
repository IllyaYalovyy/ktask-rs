//! Shared application commands. Neither terminal state nor demo fixtures enter this layer.
mod commands;
pub mod configuration;
pub mod execution;
pub mod preferences;
mod queries;
pub use queries::Event;
pub mod transfer;

use crate::storage::{Database, Error, Result};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const ROLES: [&str; 4] = ["execution", "review", "validation", "remediation"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Ai,
    Human,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ai => "ai",
            Self::Human => "human",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub title: String,
    pub goal: String,
    pub kind: Kind,
    pub criteria: Vec<String>,
    pub models: [Option<ModelConfig>; 4],
    pub attempt_limit: Option<i64>,
    pub timeout_ms: Option<i64>,
}
impl Definition {
    pub fn validate(&self) -> Result<()> {
        require(
            !self.title.trim().is_empty() && !self.goal.trim().is_empty(),
            "Title and goal are required",
        )?;
        require(
            !self.criteria.is_empty() && self.criteria.iter().all(|s| !s.trim().is_empty()),
            "At least one nonempty acceptance criterion is required",
        )?;
        require(
            self.attempt_limit.is_none_or(|n| n > 0) && self.timeout_ms.is_none_or(|n| n > 0),
            "Limits must be positive or unlimited",
        )?;
        for model in self.models.iter().flatten() {
            require(
                !model.provider.trim().is_empty() && !model.model.trim().is_empty(),
                "Provider and model are required together",
            )?;
            require(
                model.effort.as_ref().is_none_or(|s| !s.trim().is_empty()),
                "Effort must be nonempty or none",
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Revision {
    pub number: i64,
    pub lifecycle: String,
    pub definition: Definition,
    pub actor: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub id: i64,
    pub revision: i64,
    pub state: String,
    pub outcome: Option<String>,
    pub active_ms: i64,
    pub wait_reason: Option<String>,
    pub resume_at_ms: Option<i64>,
    pub session: Option<i64>,
    pub logs: Vec<String>,
    pub cost_microusd: Option<i64>,
    /// Final structured report (worker summary, supervisor verdict, or wait message).
    pub report: Option<String>,
    /// Open question text while the attempt waits for an answer.
    pub question: Option<String>,
    pub steps: Vec<Step>,
}
/// One supervisor step of an attempt (execution, check, review, validation, remediation).
#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub role: String,
    pub state: String,
    pub report: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Task {
    pub id: i64,
    pub version: i64,
    pub lifecycle: String,
    pub status: String,
    pub order_key: String,
    pub resolution_reason: Option<String>,
    pub resolved_by: Option<String>,
    pub revisions: Vec<Revision>,
    pub attempts: Vec<Attempt>,
    pub attempts_used: i64,
}
impl Task {
    pub fn definition(&self) -> &Revision {
        self.revisions
            .iter()
            .rev()
            .find(|r| r.lifecycle == "ready")
            .or_else(|| self.revisions.iter().rev().find(|r| r.lifecycle == "draft"))
            .unwrap_or_else(|| {
                self.revisions
                    .last()
                    .expect("commands create task and revision atomically")
            })
    }
}
pub struct Snapshot {
    pub queue: Vec<Task>,
    pub history: Vec<Task>,
    /// Newest first: (task id, message).
    pub events: Vec<(Option<i64>, String)>,
}
#[derive(Clone, Copy, Debug)]
pub struct VersionedTask {
    pub id: i64,
    pub version: i64,
}
#[derive(Clone, Copy)]
pub enum Resolution {
    SucceededOverride,
    FailedSkipped,
    Superseded,
}
impl Resolution {
    fn as_str(self) -> &'static str {
        match self {
            Self::SucceededOverride => "succeeded_override",
            Self::FailedSkipped => "failed_skipped",
            Self::Superseded => "superseded",
        }
    }
}

pub struct Service {
    db: Database,
    observed_data_version: i64,
    actor: Actor,
    pub project_id: i64,
    pub project_name: String,
}
#[derive(Clone, Debug)]
pub struct Actor {
    pub kind: ActorKind,
    pub name: String,
}
#[derive(Clone, Copy, Debug)]
pub enum ActorKind {
    Human,
    Ai,
}
impl ActorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Ai => "ai",
        }
    }
}
impl Default for Actor {
    fn default() -> Self {
        Self {
            kind: ActorKind::Human,
            name: "owner".into(),
        }
    }
}
impl Service {
    /// Read commands select a registered root without registering another project.
    pub fn find(db: Database, root: &Path) -> Result<Option<Self>> {
        let root = root.canonicalize()?;
        let project: Option<(i64, String)> = db
            .connection()
            .query_row(
                "SELECT id, name FROM projects WHERE root_path = ?1",
                [root.as_os_str().as_encoded_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(project.map(|(project_id, project_name)| Self {
            db,
            project_id,
            project_name,
            observed_data_version: 0,
            actor: Actor::default(),
        }))
    }
    pub fn set_actor(&mut self, actor: Actor) -> Result<()> {
        require(!actor.name.trim().is_empty(), "Actor name is required")?;
        self.actor = actor;
        Ok(())
    }
    /// The canonical working directory selects the project, never the instance data root.
    pub fn open(mut db: Database, root: &Path, name: Option<&str>) -> Result<Self> {
        let root = root.canonicalize()?;
        require(root.is_dir(), "Project root must be a directory")?;
        let bytes = root.as_os_str().as_encoded_bytes();
        let tx = db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, name FROM projects WHERE root_path = ?1",
                [bytes],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (project_id, project_name) = if let Some(project) = existing {
            require(
                name.is_none_or(|n| n == project.1),
                "Project already has a different name",
            )?;
            project
        } else {
            let default_name = root
                .file_name()
                .unwrap_or(root.as_os_str())
                .to_string_lossy();
            let name = name.unwrap_or(&default_name).trim();
            require(!name.is_empty(), "Project name is required")?;
            let taken: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE name = ?1)",
                [name],
                |r| r.get(0),
            )?;
            require(
                !taken,
                "Project name already exists; use --project-name with a unique name",
            )?;
            tx.execute(
                "INSERT INTO projects(name, root_path, created_at_ms) VALUES (?1, ?2, ?3)",
                params![name, bytes, now_ms()],
            )?;
            (tx.last_insert_rowid(), name.to_owned())
        };
        tx.commit()?;
        Ok(Self {
            db,
            observed_data_version: 0,
            actor: Actor::default(),
            project_id,
            project_name,
        })
    }
    pub fn defaults(&self) -> Result<[Option<ModelConfig>; 4]> {
        let mut models = std::array::from_fn(|_| None);
        let mut stmt = self.db.connection().prepare(
            "SELECT role, provider, model, effort FROM project_models WHERE project_id = ?1",
        )?;
        for row in stmt.query_map([self.project_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ModelConfig {
                    provider: r.get(1)?,
                    model: r.get(2)?,
                    effort: r.get(3)?,
                },
            ))
        })? {
            let (role, model) = row?;
            if let Some(index) = ROLES.iter().position(|r| *r == role) {
                models[index] = Some(model);
            }
        }
        Ok(models)
    }
    pub fn externally_changed(&self) -> Result<bool> {
        let version: i64 = self
            .db
            .connection()
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok(version != self.observed_data_version)
    }
}
pub(crate) fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Conflict(message.into()))
    }
}
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock predates epoch")
        .as_millis()
        .try_into()
        .expect("timestamp fits i64")
}
fn event(
    tx: &Transaction<'_>,
    project: i64,
    task: i64,
    now: i64,
    kind: &str,
    message: &str,
) -> Result<()> {
    tx.execute("INSERT INTO events(project_id, task_id, occurred_at_ms, kind, level, message) VALUES (?1, ?2, ?3, ?4, 'info', ?5)", params![project, task, now, kind, message])?;
    Ok(())
}
fn bump(tx: &Transaction<'_>, target: VersionedTask, now: i64) -> Result<()> {
    require(tx.execute("UPDATE tasks SET row_version = row_version + 1, updated_at_ms = max(updated_at_ms, ?3) WHERE id = ?1 AND row_version = ?2", params![target.id, target.version, now])? == 1,
        "Task changed elsewhere; cancel and reopen this form")
}

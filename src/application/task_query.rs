//! Bounded task reads that leave output and historical evidence in storage.
use super::{
    Kind, Revision, Service,
    dependency::{Dependency, task_dependencies},
    lifecycle::State,
    queries::{TASK_ORDER, latest_launch_issue, selected_revisions},
};
use crate::storage::{Error, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scope {
    #[default]
    Queue,
    History,
    All,
}

#[derive(Serialize)]
pub(crate) struct Summary {
    pub id: i64,
    version: i64,
    lifecycle: String,
    status: String,
    pub state: State,
    actions: Vec<&'static str>,
    order_key: String,
    title: String,
    kind: Kind,
    selected_revision: i64,
    pending_revision: Option<i64>,
    attempts_used: i64,
    origin_task_id: Option<i64>,
    origin_attempt_id: Option<i64>,
}

pub(crate) struct Detail {
    pub summary: Summary,
    pub selected: Revision,
    pub pending_draft: Option<Revision>,
    pub latest_attempt: Option<LatestAttempt>,
    pub dependencies: Vec<Dependency>,
    pub launch_issue: Option<String>,
    pub resolution_reason: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct LatestAttempt {
    id: i64,
    revision: i64,
    state: String,
    failure_category: Option<String>,
    wait_reason: Option<String>,
    resume_at_ms: Option<i64>,
    pub report: Option<String>,
    pub question: Option<String>,
}

// Last revision as fallback for tasks whose only revisions have been rejected or retired.
const SUMMARY_COLUMNS: &str = "
    id, row_version, lifecycle, status, wait_reason, order_key, attempts_used,
    origin_task_id, origin_attempt_id, pending_revision,
    coalesce(revision, pending_revision, (SELECT max(revision) FROM task_revisions WHERE task_id = id)),
    coalesce(title, (SELECT title FROM task_revisions WHERE task_id = id ORDER BY revision DESC LIMIT 1)),
    coalesce(kind, (SELECT kind FROM task_revisions WHERE task_id = id ORDER BY revision DESC LIMIT 1))";

fn summary(row: &Row<'_>) -> rusqlite::Result<Summary> {
    let status: String = row.get(3)?;
    let wait_reason: Option<String> = row.get(4)?;
    let state = State::of(&status, wait_reason.as_deref());
    Ok(Summary {
        id: row.get(0)?,
        version: row.get(1)?,
        lifecycle: row.get(2)?,
        status,
        state,
        actions: state
            .actions()
            .iter()
            .map(|action| action.label())
            .collect(),
        order_key: row.get(5)?,
        attempts_used: row.get(6)?,
        origin_task_id: row.get(7)?,
        origin_attempt_id: row.get(8)?,
        pending_revision: row.get(9)?,
        selected_revision: row.get(10)?,
        title: row.get(11)?,
        kind: Kind::parse(&row.get::<_, String>(12)?).unwrap_or(Kind::Human),
    })
}

fn latest_attempt(db: &Connection, id: i64) -> Result<LatestAttempt> {
    Ok(db.query_row(
        "SELECT id, revision, state, failure_category, wait_reason, resume_at_ms, coalesce(report, outcome),
            (SELECT prompt FROM questions WHERE attempt_id = a.id AND state = 'open' ORDER BY id DESC LIMIT 1)
         FROM attempts a WHERE id = ?1",
        [id],
        |row| {
            Ok(LatestAttempt {
                id: row.get(0)?,
                revision: row.get(1)?,
                state: row.get(2)?,
                failure_category: row.get(3)?,
                wait_reason: row.get(4)?,
                resume_at_ms: row.get(5)?,
                report: row.get(6)?,
                question: row.get(7)?,
            })
        },
    )?)
}

impl Service {
    pub(crate) fn task_summaries(
        &self,
        scope: Scope,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Summary>> {
        let view = match scope {
            Scope::Queue => "queue_tasks",
            Scope::History => "history_tasks",
            Scope::All => "task_details",
        };
        let mut statement = self.db.connection().prepare_cached(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM {view} WHERE project_id = ?1 ORDER BY {TASK_ORDER} LIMIT ?2 OFFSET ?3"
        ))?;
        Ok(statement
            .query_map(params![self.project_id, limit, offset], summary)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One task selected by project ID and explicit task ID, or by the canonical current view.
    pub(crate) fn task_detail(&mut self, id: Option<i64>) -> Result<Option<Detail>> {
        let tx = self.db.connection_mut().transaction()?;
        let (view, filter, parameters) = match &id {
            Some(_) => ("task_details", " AND id = ?2", params![self.project_id, id]),
            None => ("current_tasks", "", params![self.project_id]),
        };
        let row = tx.query_row(
            &format!("SELECT {SUMMARY_COLUMNS}, latest_attempt_id, resolution_reason FROM {view} WHERE project_id = ?1{filter}"),
            parameters,
            |row| Ok((summary(row)?, row.get::<_, Option<i64>>(13)?, row.get(14)?)),
        ).optional()?;
        let Some((summary, attempt_id, resolution_reason)) = row else {
            return Ok(None);
        };
        let mut revisions = selected_revisions(
            &tx,
            summary.id,
            summary.selected_revision,
            summary.pending_revision,
        )?
        .into_iter();
        let selected = revisions.next().ok_or_else(|| {
            Error::Conflict("Stored task has no revision; database needs repair".into())
        })?;
        let detail = Detail {
            selected,
            pending_draft: revisions.next(),
            latest_attempt: attempt_id.map(|id| latest_attempt(&tx, id)).transpose()?,
            dependencies: task_dependencies(&tx, self.project_id, summary.id)?,
            launch_issue: latest_launch_issue(&tx, self.project_id, summary.id)?,
            resolution_reason,
            summary,
        };
        tx.commit()?;
        Ok(Some(detail))
    }
}

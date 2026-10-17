use super::*;
use rusqlite::Connection;

#[derive(Serialize)]
pub struct Event {
    pub id: i64,
    pub task_id: Option<i64>,
    pub attempt_id: Option<i64>,
    pub occurred_at_ms: i64,
    pub kind: String,
    pub level: String,
    pub message: String,
}

const TASK_COLUMNS: &str = "id, row_version, lifecycle, status, order_key, resolution_reason, attempts_used, resolved_by_kind || ': ' || resolved_by";
fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        version: r.get(1)?,
        lifecycle: r.get(2)?,
        status: r.get(3)?,
        order_key: r.get(4)?,
        resolution_reason: r.get(5)?,
        attempts_used: r.get(6)?,
        resolved_by: r.get(7)?,
        revisions: Vec::new(),
        attempts: Vec::new(),
    })
}
fn complete_task(db: &Connection, mut task: Task) -> Result<Task> {
    task.revisions = revisions(db, task.id)?;
    require(
        !task.revisions.is_empty(),
        "Stored task has no revision; database needs repair",
    )?;
    task.attempts = attempts(db, task.id)?;
    Ok(task)
}

impl Service {
    /// Includes deleted tasks for inspection/restore, but never another project's task.
    pub fn task(&mut self, id: i64) -> Result<Option<Task>> {
        let tx = self.db.connection_mut().transaction()?;
        let task = tx
            .query_row(
                &format!(
                    "SELECT {TASK_COLUMNS} FROM task_details WHERE project_id = ?1 AND id = ?2"
                ),
                params![self.project_id, id],
                task_row,
            )
            .optional()?;
        let task = task.map(|task| complete_task(&tx, task)).transpose()?;
        tx.commit()?;
        Ok(task)
    }
    pub fn deleted(&mut self) -> Result<Vec<Task>> {
        let tx = self.db.connection_mut().transaction()?;
        let tasks = {
            let mut stmt = tx.prepare(&format!("SELECT {TASK_COLUMNS} FROM task_details WHERE project_id = ?1 AND lifecycle = 'deleted' ORDER BY closed_at_ms DESC, id DESC"))?;
            let rows = stmt
                .query_map([self.project_id], task_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .map(|task| complete_task(&tx, task))
                .collect::<Result<Vec<_>>>()?
        };
        tx.commit()?;
        Ok(tasks)
    }
    /// Cursor-based event reads for scripts; ascending IDs and an explicit bounded page.
    pub fn events_after(&self, after: i64, limit: i64) -> Result<Vec<Event>> {
        require(
            after >= 0 && (1..=1000).contains(&limit),
            "Invalid event cursor or limit",
        )?;
        let mut stmt = self.db.connection().prepare("SELECT id, task_id, attempt_id, occurred_at_ms, kind, level, message FROM events WHERE project_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3")?;
        Ok(stmt
            .query_map(params![self.project_id, after, limit], |r| {
                Ok(Event {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    attempt_id: r.get(2)?,
                    occurred_at_ms: r.get(3)?,
                    kind: r.get(4)?,
                    level: r.get(5)?,
                    message: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    /// A single read transaction keeps queue, revisions, history and events consistent.
    pub fn snapshot(&mut self) -> Result<Snapshot> {
        // Read BEFORE the snapshot: a concurrent commit may cause an extra refresh, never a missed one.
        let version: i64 = self
            .db
            .connection()
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let tx = self.db.connection_mut().transaction()?;
        let mut queue = Vec::new();
        let mut history = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, row_version, lifecycle, status, order_key, resolution_reason, attempts_used, closed_at_ms, resolved_by_kind || ': ' || resolved_by FROM task_details WHERE project_id = ?1 AND lifecycle <> 'deleted' ORDER BY CASE WHEN closed_at_ms IS NULL THEN 0 ELSE 1 END, CASE WHEN attempt_state = 'running' THEN 0 ELSE 1 END, closed_at_ms DESC, CASE WHEN closed_at_ms IS NOT NULL THEN id END DESC, order_key")?;
            let rows = stmt.query_map([self.project_id], |r| {
                Ok((
                    Task {
                        id: r.get(0)?,
                        version: r.get(1)?,
                        lifecycle: r.get(2)?,
                        status: r.get(3)?,
                        order_key: r.get(4)?,
                        resolution_reason: r.get(5)?,
                        resolved_by: r.get(8)?,
                        attempts_used: r.get(6)?,
                        revisions: Vec::new(),
                        attempts: Vec::new(),
                    },
                    r.get::<_, Option<i64>>(7)?,
                ))
            })?;
            for row in rows {
                let (mut task, closed) = row?;
                task.revisions = revisions(&tx, task.id)?;
                require(
                    !task.revisions.is_empty(),
                    "Stored task has no revision; database needs repair",
                )?;
                task.attempts = attempts(&tx, task.id)?;
                if closed.is_some() {
                    history.push(task);
                } else {
                    queue.push(task);
                }
            }
        }
        let events = {
            let mut stmt = tx.prepare(
                "SELECT message FROM events WHERE project_id = ?1 ORDER BY id DESC LIMIT 500",
            )?;
            stmt.query_map([self.project_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.commit()?;
        self.observed_data_version = version;
        Ok(Snapshot {
            queue,
            history,
            events,
        })
    }
}

fn revisions(db: &Connection, task: i64) -> Result<Vec<Revision>> {
    let mut stmt = db.prepare_cached("SELECT revision, lifecycle, title, goal, kind, attempt_limit, attempt_timeout_ms, decided_by_kind || ': ' || decided_by FROM task_revisions WHERE task_id = ?1 ORDER BY revision")?;
    let mut revisions = stmt
        .query_map([task], |r| {
            Ok(Revision {
                number: r.get(0)?,
                lifecycle: r.get(1)?,
                actor: r.get(7)?,
                definition: Definition {
                    title: r.get(2)?,
                    goal: r.get(3)?,
                    kind: if r.get::<_, String>(4)? == "ai" {
                        Kind::Ai
                    } else {
                        Kind::Human
                    },
                    attempt_limit: r.get(5)?,
                    timeout_ms: r.get(6)?,
                    criteria: Vec::new(),
                    models: std::array::from_fn(|_| None),
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for revision in &mut revisions {
        let mut criteria = db.prepare_cached("SELECT description FROM acceptance_criteria WHERE task_id = ?1 AND revision = ?2 ORDER BY ordinal")?;
        revision.definition.criteria = criteria
            .query_map(params![task, revision.number], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut models = db.prepare_cached("SELECT role, provider, model, effort FROM revision_models WHERE task_id = ?1 AND revision = ?2")?;
        for row in models.query_map(params![task, revision.number], |r| {
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
                revision.definition.models[index] = Some(model);
            }
        }
    }
    Ok(revisions)
}

fn attempts(db: &Connection, task: i64) -> Result<Vec<Attempt>> {
    let mut stmt = db.prepare_cached("SELECT a.id, a.revision, a.state, a.outcome, a.active_ms, a.wait_reason, a.resume_at_ms, (SELECT session_id FROM attempt_steps WHERE attempt_id = a.id AND role = 'execution' ORDER BY ordinal DESC LIMIT 1), (SELECT sum(cost_microusd) FROM model_calls WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id = a.id)) FROM attempts a WHERE task_id = ?1 ORDER BY number")?;
    let mut attempts = stmt
        .query_map([task], |r| {
            Ok(Attempt {
                id: r.get(0)?,
                revision: r.get(1)?,
                state: r.get(2)?,
                outcome: r.get(3)?,
                active_ms: r.get(4)?,
                wait_reason: r.get(5)?,
                resume_at_ms: r.get(6)?,
                session: r.get(7)?,
                cost_microusd: r.get(8)?,
                logs: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for attempt in &mut attempts {
        let mut output = db.prepare_cached("SELECT content FROM (SELECT sequence, content FROM attempt_output WHERE attempt_id = ?1 ORDER BY sequence DESC LIMIT 2000) ORDER BY sequence")?;
        attempt.logs = output
            .query_map([attempt.id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(attempts)
}

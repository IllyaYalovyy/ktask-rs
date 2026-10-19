use super::*;
use rusqlite::Connection;
use std::io::{Read, Seek};

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
    /// Gather the latest attempt's durable output and retained check transcripts for a proposal.
    pub fn remediation_evidence(
        &self,
        task: i64,
        artifact_root: &std::path::Path,
    ) -> Result<String> {
        let attempt: Option<i64> = self.db.connection().query_row(
            "SELECT id FROM attempts WHERE project_id=?1 AND task_id=?2 ORDER BY number DESC LIMIT 1",
            params![self.project_id, task],
            |r| r.get(0),
        ).optional()?;
        let Some(attempt) = attempt else {
            return Ok("No previous attempt output is recorded.".into());
        };
        self.attempt_evidence(attempt, artifact_root)
    }
    /// Durable output, retained check transcripts and findings of one attempt.
    /// Evidence of one attempt for a read-only judge or remediator. Each source keeps its own
    /// tail budget so verbose runner output never erases check failures or findings; together
    /// they stay well under the provider's input limit.
    pub fn attempt_evidence(
        &self,
        attempt: i64,
        artifact_root: &std::path::Path,
    ) -> Result<String> {
        let mut evidence = String::from(
            "Latest attempt output, including execution, checks, review, and validation:\n",
        );
        let mut output = self.db.connection().prepare("SELECT stream,content FROM attempt_output WHERE attempt_id=?1 ORDER BY sequence DESC LIMIT 2000")?;
        let rows = output.query_map([attempt], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut chunks = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        chunks.reverse();
        for (stream, text) in chunks {
            evidence.push_str(&format!("\n[{stream}] {text}"));
            retain_tail(&mut evidence, OUTPUT_TAIL);
        }
        let mut transcripts = String::new();
        let mut artifacts = self.db.connection().prepare("SELECT name,relative_path FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL ORDER BY id")?;
        let paths = artifacts.query_map([attempt], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for (name, relative) in paths.collect::<rusqlite::Result<Vec<_>>>()? {
            let relative = std::path::Path::new(&relative);
            if relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                continue;
            }
            let path = artifact_root.join(relative);
            if let Ok(mut file) = std::fs::File::open(path) {
                let start = file
                    .metadata()?
                    .len()
                    .saturating_sub(TRANSCRIPT_TAIL as u64);
                file.seek(std::io::SeekFrom::Start(start))?;
                let mut bytes = Vec::new();
                file.take(TRANSCRIPT_TAIL as u64).read_to_end(&mut bytes)?;
                transcripts.push_str(&format!(
                    "\n\nFailed check transcript ({name}):\n{}",
                    String::from_utf8_lossy(&bytes)
                ));
                retain_tail(&mut transcripts, TRANSCRIPT_TAIL);
            }
        }
        evidence.push_str(&transcripts);
        // Structured findings can be absent from output, or precede a verbose final check.
        let mut reports = String::new();
        let mut stmt = self.db.connection().prepare(
            "SELECT 'attempt', coalesce(report,outcome,'') FROM attempts WHERE id=?1
             UNION ALL SELECT role, report FROM attempt_steps WHERE attempt_id=?1 AND report IS NOT NULL
             UNION ALL SELECT s.role || ' criterion ' || r.criterion_ordinal, r.evidence
             FROM criterion_results r JOIN attempt_steps s ON s.id=r.step_id
             WHERE s.attempt_id=?1 AND r.evidence IS NOT NULL"
        )?;
        for row in stmt.query_map([attempt], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (role, report) = row?;
            reports.push_str(&format!("\n[{role} finding] {report}"));
            retain_tail(&mut reports, FINDINGS_TAIL);
        }
        evidence.push_str(&reports);
        Ok(evidence)
    }
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
                "SELECT task_id, message FROM events WHERE project_id = ?1 ORDER BY id DESC LIMIT 500",
            )?;
            stmt.query_map([self.project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
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
    let mut stmt = db.prepare_cached("SELECT revision, lifecycle, title, goal, kind, attempt_limit, attempt_timeout_ms, coalesce(decided_by_kind, created_by_kind) || ': ' || coalesce(decided_by, created_by) FROM task_revisions WHERE task_id = ?1 ORDER BY revision")?;
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
    let mut stmt = db.prepare_cached("SELECT a.id, a.revision, a.state, a.outcome, a.active_ms, a.wait_reason, a.resume_at_ms, (SELECT session_id FROM attempt_steps WHERE attempt_id = a.id AND role = 'execution' ORDER BY ordinal DESC LIMIT 1), (SELECT sum(cost_microusd) FROM model_calls WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id = a.id)), a.report, (SELECT prompt FROM questions WHERE attempt_id = a.id AND state = 'open' ORDER BY id DESC LIMIT 1) FROM attempts a WHERE task_id = ?1 ORDER BY number")?;
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
                report: r.get(9)?,
                question: r.get(10)?,
                steps: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for attempt in &mut attempts {
        let mut output = db.prepare_cached("SELECT content FROM (SELECT sequence, content FROM attempt_output WHERE attempt_id = ?1 ORDER BY sequence DESC LIMIT 2000) ORDER BY sequence")?;
        attempt.logs = output
            .query_map([attempt.id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut steps = db.prepare_cached(
            "SELECT role, state, report FROM attempt_steps WHERE attempt_id = ?1 ORDER BY ordinal",
        )?;
        attempt.steps = steps
            .query_map([attempt.id], |r| {
                Ok(Step {
                    role: r.get(0)?,
                    state: r.get(1)?,
                    report: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(attempts)
}

/// Keep the newest evidence, advancing over any partial UTF-8 character.
/// Codex rejects a turn above 1,048,576 characters; the three tails sum to 768 KiB of bytes,
/// which is never more characters than that.
const OUTPUT_TAIL: usize = 384 * 1024;
const TRANSCRIPT_TAIL: usize = 256 * 1024;
const FINDINGS_TAIL: usize = 128 * 1024;

fn retain_tail(text: &mut String, limit: usize) {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if start > 0 {
        text.drain(..start);
    }
}

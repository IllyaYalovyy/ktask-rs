//! What an investigator reads about a project: a snapshot of ktask's records written as
//! files, because its sandbox cannot open the database. Nothing here changes a record.
use super::*;
use rusqlite::{Connection, types::ValueRef};
use serde_json::{Map, Value, json};
use std::{
    fs::{self, File},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
};

const EVENTS: i64 = 2000;
const ATTEMPTS: i64 = 200;
const CLOSED_TASKS: i64 = 100;
/// The most recent output and runner log copied for one source.
const TAIL_BYTES: usize = 1024 * 1024;
/// Failed or waiting tasks detailed beside the focus and current task.
const TROUBLED_TASKS: usize = 3;

/// Writes every row of `sql` as one JSON object per line, named by its columns.
fn rows(db: &Connection, path: &Path, sql: &str, params: impl rusqlite::Params) -> Result<usize> {
    let mut out = BufWriter::new(File::create(path)?);
    let mut statement = db.prepare(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|name| (*name).into())
        .collect();
    let mut found = statement.query(params)?;
    let mut count = 0;
    while let Some(row) = found.next()? {
        let mut record = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(text) | ValueRef::Blob(text) => {
                    json!(String::from_utf8_lossy(text))
                }
            };
            record.insert(name.clone(), value);
        }
        writeln!(out, "{}", Value::Object(record))?;
        count += 1;
    }
    out.flush()?;
    Ok(count)
}

fn pretty(path: &Path, value: &Value) -> Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(|e| Error::Conflict(e.to_string()))?,
    )?;
    Ok(())
}

/// The last [`TAIL_BYTES`] of the file at `path`; None when there is none.
fn tail(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES as u64)))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES as u64).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

impl Service {
    /// The attempt's latest output, one `#sequence stream: text` line per record.
    fn output_excerpt(&self, attempt: i64) -> Result<String> {
        let mut statement = self.db.connection().prepare(
            "SELECT sequence,stream,content FROM attempt_output WHERE attempt_id=?1 ORDER BY sequence DESC",
        )?;
        let mut lines = Vec::new();
        let mut size = 0;
        let mut found = statement.query([attempt])?;
        while let Some(row) = found.next()? {
            let line = format!(
                "#{} {}: {}\n",
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?.trim_end()
            );
            size += line.len();
            if size > TAIL_BYTES && !lines.is_empty() {
                lines.push("[earlier output omitted]\n".into());
                break;
            }
            lines.push(line);
        }
        lines.reverse();
        Ok(lines.concat())
    }

    fn configuration_value(&self, config: &Path) -> Result<Value> {
        let checks: Vec<Value> = self
            .project_checks()?
            .into_iter()
            .map(|check| {
                json!({"name":check.name,"command":check.command,"directory":check.directory,
                    "timeout_ms":check.timeout_ms,"enabled":check.enabled})
            })
            .collect();
        let tools: Vec<Value> = self
            .tools()?
            .into_iter()
            .filter(|tool| tool.bound)
            .map(|tool| json!(tool))
            .collect();
        Ok(json!({
            "checks": checks,
            "delivery": self.delivery_config()?,
            "models": self.effective_models()?,
            "preferences": self.preferences(config)?,
            "agent_network": self.agent_network()?,
            "check_before_task": self.check_before_task()?,
            "retention": self.retention_policy()?,
            "investigation_limits": self.investigation_limits()?,
            "bound_tools": tools,
        }))
    }

    /// One task's definition, history and evidence under `folder`.
    fn task_packet(
        &mut self,
        id: i64,
        folder: &Path,
        state: &Path,
        index: &mut String,
    ) -> Result<()> {
        let Some(task) = self.task(id)? else {
            return Err(Error::Conflict(format!(
                "Task #{id} does not exist in this project"
            )));
        };
        let folder = folder.join(format!("task-{id}"));
        fs::create_dir_all(&folder)?;
        let revision = task.definition();
        let definition = &revision.definition;
        pretty(
            &folder.join("task.json"),
            &json!({
                "id": task.id, "state": task.state(), "lifecycle": task.lifecycle,
                "revision": revision.number, "title": definition.title, "goal": definition.goal,
                "criteria": definition.criteria, "kind": definition.kind,
                "category": definition.category, "attempt_limit": definition.attempt_limit,
                "attempts_used": task.attempts_used,
                "resolution_reason": task.resolution_reason,
                "lifecycle_events": task.lifecycle_events,
                "dependencies": self.dependencies(id)?,
                "open_question": self.open_question(id)?.map(|(_, prompt)| prompt),
                "decisions_and_answers": self.failure_decisions(id)?,
            }),
        )?;
        index.push_str(&format!(
            "task-{id}/task.json: #{id} {} ({})\n",
            definition.title,
            task.state().label()
        ));
        for attempt in task.attempts.iter().rev().take(3) {
            let output = self.output_excerpt(attempt.id)?;
            if !output.is_empty() {
                let name = format!("output-attempt-{}.log", attempt.id);
                fs::write(folder.join(&name), output)?;
                index.push_str(&format!(
                    "task-{id}/{name}: latest output of attempt #{}\n",
                    attempt.id
                ));
            }
        }
        if let Some(latest) = task.attempts.last() {
            let context = format!(
                "{}\nRecorded answers and decisions:\n{}",
                self.attempt_evidence(latest.id, state)?,
                self.failure_decisions(id)?
            );
            self.review_bundle(latest.id, state, &folder, "not-computed", &context)?;
            index.push_str(&format!(
                "task-{id}/steps.jsonl, step-*.txt, artifacts.jsonl, criteria.jsonl, workflow.txt: evidence of its attempts (latest #{})\n",
                latest.id
            ));
        }
        Ok(())
    }

    /// Writes the snapshot of this project into `folder` and returns its index: which file
    /// holds what. `focus` is the task the owner asked about; `state` and `config` are the
    /// instance's roots.
    pub(crate) fn investigation_packet(
        &mut self,
        folder: &Path,
        state: &Path,
        config: &Path,
        focus: Option<i64>,
        runner_alive: bool,
    ) -> Result<String> {
        fs::create_dir_all(folder)?;
        let project = self.project_id;
        let current = self.current_task()?.map(|task| task.id);
        let mut index = String::new();
        let session = self
            .execution_session()?
            .map(|s| json!({"mode":s.mode,"state":s.state,"reason":s.reason}));
        pretty(
            &folder.join("overview.json"),
            &json!({
                "project": self.project_name,
                "root": self.project_root()?.to_string_lossy(),
                "snapshot_at_ms": now_ms(),
                "runner_alive": runner_alive,
                "execution_session": session,
                "current_task": current,
                "focus_task": focus,
            }),
        )?;
        index.push_str("overview.json: project, runner and current task\n");
        pretty(
            &folder.join("configuration.json"),
            &self.configuration_value(config)?,
        )?;
        index
            .push_str("configuration.json: checks, delivery, models, preferences, limits, tools\n");
        let db = self.db.connection();
        let listed = rows(
            db,
            &folder.join("tasks.jsonl"),
            "SELECT id,status,lifecycle,kind,title,attempts_used,latest_attempt_id,attempt_state,wait_reason,closed_at_ms,resolution_reason FROM task_details WHERE project_id=?1 AND (closed_at_ms IS NULL OR id IN (SELECT id FROM tasks WHERE project_id=?1 AND closed_at_ms IS NOT NULL ORDER BY closed_at_ms DESC LIMIT ?2)) ORDER BY closed_at_ms IS NOT NULL, order_key",
            params![project, CLOSED_TASKS],
        )?;
        index.push_str(&format!(
            "tasks.jsonl: {listed} tasks, unfinished first in execution order\n"
        ));
        let count = rows(
            db,
            &folder.join("attempts.jsonl"),
            "SELECT id,task_id,revision,number,state,failure_category,wait_reason,started_at_ms,finished_at_ms,resume_at_ms,base_commit,head_commit,outcome,report FROM attempts WHERE project_id=?1 ORDER BY id DESC LIMIT ?2",
            params![project, ATTEMPTS],
        )?;
        index.push_str(&format!(
            "attempts.jsonl: {count} latest attempts, newest first\n"
        ));
        let count = rows(
            db,
            &folder.join("events.jsonl"),
            "SELECT * FROM (SELECT id,task_id,attempt_id,occurred_at_ms,kind,level,message FROM events WHERE project_id=?1 ORDER BY id DESC LIMIT ?2) ORDER BY id",
            params![project, EVENTS],
        )?;
        index.push_str(&format!(
            "events.jsonl: {count} latest events, oldest first; cite them by id\n"
        ));
        let count = rows(
            db,
            &folder.join("dependencies.jsonl"),
            "SELECT id,task_id,attempt_id,kind,item,required_version,scope,stage,state,reproduction,observed,checked_at_ms FROM task_requirements WHERE project_id=?1 ORDER BY id",
            params![project],
        )?;
        index.push_str(&format!(
            "dependencies.jsonl: {count} declared prerequisites and their probes\n"
        ));
        let log = self.db.state_file(&format!("runner-{project}.log"))?;
        match tail(&log)? {
            Some(bytes) => {
                fs::write(folder.join("runner.log"), bytes)?;
                index.push_str("runner.log: latest runner log; cite it as runner.log:LINE\n");
            }
            None => index.push_str("No runner log exists for this project.\n"),
        }
        let mut detailed: Vec<i64> = focus.into_iter().chain(current).collect();
        let troubled: Vec<i64> = db
            .prepare("SELECT id FROM queue_tasks WHERE project_id=?1 AND status IN ('failed','waiting','paused','dependency_wait') ORDER BY order_key")?
            .query_map([project], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        // A project with nothing wrong in its queue still shows what ran last.
        let latest: Option<i64> = db
            .query_row(
                "SELECT task_id FROM attempts WHERE project_id=?1 ORDER BY id DESC LIMIT 1",
                [project],
                |r| r.get(0),
            )
            .optional()?;
        detailed.extend(troubled.into_iter().take(TROUBLED_TASKS));
        detailed.extend(latest);
        let mut seen = std::collections::BTreeSet::new();
        detailed.retain(|id| seen.insert(*id));
        for id in detailed {
            self.task_packet(id, folder, state, &mut index)?;
        }
        Ok(index)
    }
}

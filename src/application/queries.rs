use super::output_text::{CHUNK_BYTES, read_output_range, visible_controls};
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

const TASK_COLUMNS: &str = "id, row_version, lifecycle, status, order_key, resolution_reason, attempts_used, resolved_by_kind || ': ' || resolved_by, origin_task_id, origin_attempt_id";
pub struct WaitingResume {
    pub resume_at_ms: Option<i64>,
    pub eligible: bool,
    pub remaining_seconds: i64,
}
pub struct ProjectTiming {
    pub attempts: i64,
    pub failed: i64,
    pub active_ms: i64,
    pub waiting_ms: i64,
}
fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        version: r.get(1)?,
        lifecycle: r.get(2)?,
        status: r.get(3)?,
        order_key: r.get(4)?,
        origin_task_id: r.get(8)?,
        origin_attempt_id: r.get(9)?,
        follow_ups: Vec::new(),
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
    task.attempts = attempts(db, task.id, true, true)?;
    task.follow_ups = follow_ups(db, task.id)?;
    Ok(task)
}
fn follow_ups(db: &Connection, task: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = db.prepare_cached("SELECT id, coalesce(title, (SELECT title FROM task_revisions WHERE task_id=t.id ORDER BY revision DESC LIMIT 1)) FROM task_details t WHERE origin_task_id=?1 AND lifecycle<>'deleted' ORDER BY order_key")?;
    Ok(stmt
        .query_map([task], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

impl Service {
    pub fn project_usage(&self) -> Result<super::usage_report::UsageTotals> {
        super::usage_report::project(self.db.connection(), self.project_id)
    }
    pub fn project_timing(&self) -> Result<ProjectTiming> {
        let db = self.db.connection();
        let mut timing = ProjectTiming {
            attempts: 0,
            failed: 0,
            active_ms: 0,
            waiting_ms: 0,
        };
        let mut stmt = db.prepare("SELECT id,state,active_ms FROM attempts WHERE project_id=?1")?;
        for row in stmt.query_map([self.project_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (id, state, active) = row?;
            timing.attempts += 1;
            timing.failed += i64::from(matches!(state.as_str(), "failed" | "interrupted"));
            timing.active_ms = timing.active_ms.saturating_add(active);
            timing.waiting_ms = timing.waiting_ms.saturating_add(waiting_time(db, id)?);
        }
        Ok(timing)
    }
    const OUTPUT_ROWS: usize = 80;

    pub fn output_watermark(&self, attempt: i64) -> Result<i64> {
        Ok(self.db.connection().query_row(
            "SELECT coalesce(max(sequence),0) FROM attempt_output WHERE attempt_id=?1",
            [attempt],
            |r| r.get(0),
        )?)
    }

    pub fn output_page_count_at(&self, attempt: i64, watermark: i64) -> Result<usize> {
        let count: i64 = self.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1 AND sequence<=?2",
            params![attempt, watermark],
            |r| r.get(0),
        )?;
        Ok((count.max(1) as usize).div_ceil(Self::OUTPUT_ROWS))
    }

    pub fn output_chunk_count_at(
        &self,
        attempt: i64,
        page: usize,
        watermark: i64,
    ) -> Result<usize> {
        let mut statement = self.db.connection().prepare(
            "SELECT length(CAST(content AS BLOB)),substr(CAST(content AS BLOB),-4) FROM attempt_output WHERE attempt_id=?1 AND sequence<=?4 ORDER BY sequence DESC LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            params![
                attempt,
                Self::OUTPUT_ROWS as i64,
                page.saturating_mul(Self::OUTPUT_ROWS) as i64,
                watermark
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        let mut chunks = 1;
        for row in rows {
            let (length, tail) = row?;
            // A final codepoint may extend past the byte boundary while still
            // belonging to the preceding chunk. Only its first byte counts.
            if let Some(last_start) = tail.iter().rposition(|byte| byte & 0xc0 != 0x80) {
                let position = length as usize - tail.len() + last_start;
                chunks = chunks.max(position / CHUNK_BYTES + 1);
            }
        }
        Ok(chunks)
    }

    pub fn artifact_count(&self, attempt: i64) -> Result<usize> {
        let count: i64 = self.db.connection().query_row(
            "SELECT count(*) FROM artifacts WHERE attempt_id=?1",
            [attempt],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn transcript_page_count(
        &self,
        attempt: i64,
        artifact: usize,
        root: &std::path::Path,
    ) -> Result<usize> {
        let path: Option<String> = self.db.connection().query_row("SELECT relative_path FROM artifacts WHERE attempt_id=?1 ORDER BY id LIMIT 1 OFFSET ?2", params![attempt,artifact as i64], |r| r.get(0)).optional()?;
        let Some(path) = path else {
            return Ok(1);
        };
        let Some(path) = retained_artifact_path(root, &path) else {
            return Ok(1);
        };
        let Ok(metadata) = std::fs::metadata(path) else {
            return Ok(1);
        };
        Ok(metadata.len().max(1).div_ceil(8192) as usize)
    }
    /// Read one bounded page for the TUI. This path never changes attempt state.
    pub fn browse_output_at(
        &self,
        attempt: i64,
        page: usize,
        chunk: usize,
        selected_match: Option<(i64, &str)>,
        watermark: i64,
    ) -> Result<String> {
        let db = self.db.connection();
        let total: i64 = db.query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1 AND sequence<=?2",
            params![attempt, watermark],
            |r| r.get(0),
        )?;
        let offset = page.saturating_mul(Self::OUTPUT_ROWS);
        let mut stmt = db.prepare(
            "SELECT sequence,stream,length(CAST(content AS BLOB)) FROM attempt_output WHERE attempt_id=?1 AND sequence<=?4 ORDER BY sequence DESC LIMIT ?3 OFFSET ?2"
        )?;
        let rows = stmt
            .query_map(
                params![attempt, offset as i64, Self::OUTPUT_ROWS as i64, watermark],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let chunks = self.output_chunk_count_at(attempt, page, watermark)?;
        let mut text = format!(
            "Output page {} / {} · chunk {} / {} · {} records · newest first · snapshot #{watermark}\n",
            page + 1,
            (total.max(1) as usize).div_ceil(Self::OUTPUT_ROWS),
            chunk + 1,
            chunks,
            total
        );
        if rows.is_empty() {
            text.push_str("No output records on this page.\n");
        }
        let byte_offset = chunk.saturating_mul(CHUNK_BYTES);
        // A match can straddle a chunk boundary. Only inspect bounded context at
        // its landing chunk; never scan or load the entire output record here.
        if let Some((sequence, needle)) = selected_match.filter(|(sequence, needle)| {
            !needle.is_empty() && rows.iter().any(|row| row.0 == *sequence)
        }) {
            let context = read_output_range(
                db,
                attempt,
                sequence,
                byte_offset,
                CHUNK_BYTES.saturating_add(needle.len().min(4096)),
            )?;
            if let Some(at) = context
                .content
                .to_ascii_lowercase()
                .find(&needle.to_ascii_lowercase())
                && context.start + at < byte_offset.saturating_add(CHUNK_BYTES)
                && context.start + at + needle.len() > byte_offset.saturating_add(CHUNK_BYTES)
            {
                let excerpt = visible_controls(&context.content[at..at + needle.len()]);
                text.push_str(&format!("\nMatch in #{sequence}: {excerpt}\n"));
            }
        }
        for (sequence, stream, length) in rows {
            let slice = read_output_range(db, attempt, sequence, byte_offset, CHUNK_BYTES)?;
            let content = visible_controls(&slice.content);
            if chunk > 0 && content.is_empty() {
                continue;
            }
            let continuation = if chunk > 0 { "[continued] " } else { "" };
            text.push_str(&format!("\n#{sequence} [{stream}] {continuation}{content}"));
            if length as usize > slice.start.saturating_add(slice.content.len()) {
                text.push_str("\n[continues in next chunk]");
            }
        }
        Ok(text)
    }

    /// Structured results are kept independent of the raw output tail.
    pub fn browse_results(&self, attempt: i64) -> Result<String> {
        let db = self.db.connection();
        let mut text = String::from("Role steps, findings and acceptance results\n");
        let mut stmt = db.prepare("SELECT s.id,s.ordinal,s.role,s.state,coalesce(c.name,''),coalesce(s.report,''),s.command,s.exit_code,s.passed_count,s.failed_count,s.ignored_count FROM attempt_steps s LEFT JOIN project_checks c ON c.id=s.project_check_id WHERE s.attempt_id=?1 ORDER BY s.ordinal")?;
        let steps = stmt
            .query_map([attempt], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if steps.is_empty() {
            text.push_str("No role steps recorded.\n");
        }
        for (id, ordinal, role, state, check, report, command, exit, passed, failed, ignored) in
            steps
        {
            text.push_str(&format!(
                "\nStep {ordinal} · {role}{} · {state}\n",
                if check.is_empty() {
                    String::new()
                } else {
                    format!(" · {check}")
                }
            ));
            if !report.is_empty() {
                text.push_str(&format!("{report}\n"));
            }
            if let Some(command) = command {
                text.push_str(&format!("Command: {command}\n"));
            }
            if let Some(exit) = exit {
                text.push_str(&format!("Exit code: {exit}\n"));
            }
            if let (Some(passed), Some(failed), Some(ignored)) = (passed, failed, ignored) {
                text.push_str(&format!(
                    "Check counts: {passed} passed, {failed} failed, {ignored} ignored\n"
                ));
            }
            let mut criteria = db.prepare("SELECT criterion_ordinal,state,evidence FROM criterion_results WHERE step_id=?1 ORDER BY criterion_ordinal")?;
            for row in criteria.query_map([id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })? {
                let (number, state, evidence) = row?;
                text.push_str(&format!(
                    "Criterion {number} · {state}: {}\n",
                    evidence.unwrap_or_else(|| "No evidence recorded".into())
                ));
            }
        }
        Ok(text)
    }

    pub fn browse_transcript(
        &self,
        attempt: i64,
        artifact: usize,
        page: usize,
        root: &std::path::Path,
    ) -> Result<String> {
        let db = self.db.connection();
        let mut stmt = db.prepare("SELECT name,relative_path,size_bytes,pruned_at_ms FROM artifacts WHERE attempt_id=?1 ORDER BY id")?;
        let artifacts = stmt
            .query_map([attempt], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut text = format!(
            "Supervisor check transcripts · {} artifact(s)\n",
            artifacts.len()
        );
        if artifacts.is_empty() {
            text.push_str("No transcript artifact recorded.\n");
            return Ok(text);
        }
        let index = artifact.min(artifacts.len() - 1);
        let (name, relative, size, pruned) = &artifacts[index];
        text.push_str(&format!(
            "Artifact {} / {} · {name} · {size} bytes\n",
            index + 1,
            artifacts.len()
        ));
        if pruned.is_some() {
            text.push_str("PRUNED: transcript content is no longer retained.\n");
            return Ok(text);
        }
        let Some(path) = retained_artifact_path(root, relative) else {
            text.push_str("MISSING: transcript file or safe path is absent.\n");
            return Ok(text);
        };
        let Ok(mut file) = std::fs::File::open(&path) else {
            text.push_str("MISSING: transcript file is absent.\n");
            return Ok(text);
        };
        let actual = file.metadata()?.len();
        let start = utf8_boundary(&mut file, (page as u64).saturating_mul(8192).min(actual))?;
        let end = utf8_boundary(
            &mut file,
            ((page as u64).saturating_add(1))
                .saturating_mul(8192)
                .min(actual),
        )?;
        text.push_str(&format!(
            "Chunk {} / {}{}\n",
            page + 1,
            actual.max(1).div_ceil(8192),
            if actual != *size as u64 {
                " · SIZE MISMATCH"
            } else {
                ""
            }
        ));
        file.seek(std::io::SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end.saturating_sub(start))
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            text.push_str("No content in this chunk.\n");
        }
        text.push_str(&String::from_utf8_lossy(&bytes));
        Ok(text)
    }
    /// Persisted waiting-attempt timing, read without changing the attempt or its guards.
    pub fn waiting_resume(&self, task: i64) -> Result<Option<WaitingResume>> {
        self.db
            .connection()
            .query_row(
                "SELECT a.resume_at_ms, (a.waiting_since_ms IS NULL OR a.waiting_since_ms <= ?3)
                AND (a.resume_at_ms IS NULL OR a.resume_at_ms <= ?3)
                , (max(coalesce(a.resume_at_ms,0),coalesce(a.waiting_since_ms,0),?3)-?3+999)/1000
             FROM attempts a JOIN current_tasks t ON t.latest_attempt_id=a.id
             WHERE t.project_id=?1 AND t.id=?2 AND a.state='waiting'",
                params![self.project_id, task, now_ms()],
                |row| {
                    Ok(WaitingResume {
                        resume_at_ms: row.get(0)?,
                        eligible: row.get(1)?,
                        remaining_seconds: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
    /// Launch diagnostics are durable even when preflight stops before an attempt exists.
    pub fn record_launch_result(&mut self, task: i64, message: &str) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO events(project_id,task_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,'launch',?4,?5)",
            params![self.project_id, task, now_ms(), if message.is_empty() { "info" } else { "error" }, if message.is_empty() { "Supervisor started" } else { message }],
        )?;
        Ok(())
    }

    pub fn latest_launch_issue(&self, task: i64) -> Result<Option<String>> {
        self.db.connection().query_row(
            "SELECT level,message FROM events WHERE project_id=?1 AND task_id=?2 AND kind='launch' ORDER BY id DESC LIMIT 1",
            params![self.project_id, task], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ).optional().map(|value| value.and_then(|(level, message)| (level == "error").then_some(message))).map_err(Into::into)
    }
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
        let mut evidence = self.attempt_evidence(attempt, artifact_root)?;
        evidence.push_str(&self.failure_decisions(task)?);
        Ok(evidence)
    }
    pub fn failure_decisions(&self, task: i64) -> Result<String> {
        let mut evidence = String::new();
        let mut stmt = self.db.connection().prepare(
            "SELECT message FROM events WHERE project_id=?1 AND task_id=?2 AND kind IN ('remediation.repair','remediation.ask') ORDER BY id DESC LIMIT 10",
        )?;
        for message in stmt.query_map(params![self.project_id, task], |row| {
            row.get::<_, String>(0)
        })? {
            evidence.push_str(&format!("\nInteractive failure judgment:\n{}", message?));
        }
        let mut stmt = self.db.connection().prepare(
            "SELECT q.prompt,q.answer FROM questions q JOIN attempts a ON a.id=q.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 AND q.state='answered' ORDER BY q.id DESC LIMIT 10",
        )?;
        for answer in stmt.query_map(params![self.project_id, task], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (question, answer) = answer?;
            evidence.push_str(&format!("\nOwner answer to '{question}': {answer}"));
        }
        Ok(evidence)
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
            if let Ok(mut file) = std::fs::File::open(&path) {
                let start = file
                    .metadata()?
                    .len()
                    .saturating_sub(TRANSCRIPT_TAIL as u64);
                file.seek(std::io::SeekFrom::Start(start))?;
                let mut bytes = Vec::new();
                file.take(TRANSCRIPT_TAIL as u64).read_to_end(&mut bytes)?;
                transcripts.push_str(&format!(
                    "\n\nSupervisor check transcript ({name}, {}):\n{}",
                    path.display(),
                    String::from_utf8_lossy(&bytes)
                ));
                retain_tail(&mut transcripts, TRANSCRIPT_TAIL);
            }
        }
        evidence.push_str(&transcripts);
        // Decisions can precede megabytes of check output. Keep their original
        // structured responses available independently of the general log tail.
        let mut decisions = String::new();
        let mut stmt = self.db.connection().prepare(
            "SELECT content FROM attempt_output o WHERE attempt_id=?1 AND stream='stdout'
             AND CASE WHEN json_valid(content) THEN json_type(content,'$.judgment')='object' ELSE 0 END
             AND EXISTS (SELECT 1 FROM attempt_steps s WHERE s.attempt_id=o.attempt_id
                 AND s.role='remediation' AND s.state='succeeded' AND s.report LIKE 'Repair:%'
                 AND o.recorded_at_ms BETWEEN s.started_at_ms AND s.finished_at_ms)
             ORDER BY sequence",
        )?;
        for row in stmt.query_map([attempt], |r| r.get::<_, String>(0))? {
            decisions.push_str(&format!("\nRecorded judgment output:\n{}", row?));
            retain_tail(&mut decisions, FINDINGS_TAIL);
        }
        evidence.push_str(&decisions);
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
        self.snapshot_with_logs(true)
    }

    /// The TUI keeps only the live pane's tail; its evidence browser loads older output on demand.
    pub fn snapshot_brief(&mut self) -> Result<Snapshot> {
        self.snapshot_with_logs(false)
    }

    fn snapshot_with_logs(&mut self, historical_logs: bool) -> Result<Snapshot> {
        // Read BEFORE the snapshot: a concurrent commit may cause an extra refresh, never a missed one.
        let version: i64 = self
            .db
            .connection()
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let tx = self.db.connection_mut().transaction()?;
        let mut queue = Vec::new();
        let mut history = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, row_version, lifecycle, status, order_key, resolution_reason, attempts_used, closed_at_ms, resolved_by_kind || ': ' || resolved_by, origin_task_id, origin_attempt_id FROM task_details WHERE project_id = ?1 AND lifecycle <> 'deleted' ORDER BY CASE WHEN closed_at_ms IS NULL THEN 0 ELSE 1 END, CASE WHEN attempt_state = 'running' THEN 0 ELSE 1 END, closed_at_ms DESC, CASE WHEN closed_at_ms IS NOT NULL THEN id END DESC, order_key")?;
            let rows = stmt.query_map([self.project_id], |r| {
                Ok((
                    Task {
                        id: r.get(0)?,
                        version: r.get(1)?,
                        lifecycle: r.get(2)?,
                        status: r.get(3)?,
                        order_key: r.get(4)?,
                        origin_task_id: r.get(9)?,
                        origin_attempt_id: r.get(10)?,
                        follow_ups: Vec::new(),
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
                let live_logs = closed.is_none() && queue.is_empty();
                task.attempts = attempts(&tx, task.id, historical_logs, live_logs)?;
                task.follow_ups = follow_ups(&tx, task.id)?;
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

/// Move a nominal byte boundary backward to the start of a UTF-8 codepoint.
/// Adjacent chunks then meet without replacement characters or duplicated bytes.
fn utf8_boundary(file: &mut std::fs::File, mut position: u64) -> std::io::Result<u64> {
    let length = file.metadata()?.len();
    if position >= length {
        return Ok(length);
    }
    for _ in 0..3 {
        file.seek(std::io::SeekFrom::Start(position))?;
        let mut byte = [0];
        file.read_exact(&mut byte)?;
        if byte[0] & 0b1100_0000 != 0b1000_0000 {
            break;
        }
        position = position.saturating_sub(1);
    }
    Ok(position)
}

pub(super) fn retained_artifact_path(
    root: &std::path::Path,
    relative: &str,
) -> Option<std::path::PathBuf> {
    let relative = std::path::Path::new(relative);
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = root.join(relative).canonicalize().ok()?;
    path.starts_with(&root).then_some(path)
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

fn attempts(
    db: &Connection,
    task: i64,
    historical_logs: bool,
    live_logs: bool,
) -> Result<Vec<Attempt>> {
    let mut stmt = db.prepare_cached("SELECT a.id, a.revision, a.state, a.outcome, a.active_ms, a.wait_reason, a.resume_at_ms, (SELECT session_id FROM attempt_steps WHERE attempt_id = a.id AND role = 'execution' ORDER BY ordinal DESC LIMIT 1), (SELECT sum(cost_microusd) FROM model_calls WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id = a.id)), a.report, (SELECT prompt FROM questions WHERE attempt_id = a.id AND state = 'open' ORDER BY id DESC LIMIT 1) FROM attempts a WHERE task_id = ?1 ORDER BY number")?;
    let mut attempts = stmt
        .query_map([task], |r| {
            Ok(Attempt {
                id: r.get(0)?,
                revision: r.get(1)?,
                state: r.get(2)?,
                outcome: r.get(3)?,
                active_ms: r.get(4)?,
                waiting_ms: 0,
                usage: Default::default(),
                role_usage: Vec::new(),
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
    let latest = attempts.last().map(|attempt| attempt.id);
    for attempt in &mut attempts {
        (attempt.usage, attempt.role_usage) = super::usage_report::attempt(db, attempt.id)?;
        attempt.cost_microusd = attempt.usage.cost_microusd;
        attempt.waiting_ms = waiting_time(db, attempt.id)?;
        // The live pane only follows the current attempt. Historical output is
        // fetched in bounded pages when its evidence browser is opened.
        if historical_logs || (live_logs && Some(attempt.id) == latest) {
            let mut output = db.prepare_cached("SELECT content FROM (SELECT sequence, content FROM attempt_output WHERE attempt_id = ?1 ORDER BY sequence DESC LIMIT 2000) ORDER BY sequence")?;
            attempt.logs = output
                .query_map([attempt.id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        let mut steps = db.prepare_cached(
            "SELECT s.role, s.state, s.report, c.name, s.passed_count, s.failed_count, s.ignored_count
             FROM attempt_steps s LEFT JOIN project_checks c ON c.id=s.project_check_id
             WHERE s.attempt_id = ?1 ORDER BY s.ordinal",
        )?;
        attempt.steps = steps
            .query_map([attempt.id], |r| {
                Ok(Step {
                    role: r.get(0)?,
                    state: r.get(1)?,
                    report: r.get(2)?,
                    check_name: r.get(3)?,
                    counts: match r.get::<_, Option<i64>>(4)? {
                        Some(passed) => Some(CheckCounts {
                            passed,
                            failed: r.get(5)?,
                            ignored: r.get(6)?,
                        }),
                        None => None,
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(attempts)
}

fn waiting_time(db: &Connection, attempt: i64) -> Result<i64> {
    let finished: Option<i64> = db.query_row(
        "SELECT finished_at_ms FROM attempts WHERE id=?1",
        [attempt],
        |r| r.get(0),
    )?;
    let mut total: i64 = 0;
    let mut started: Option<i64> = None;
    let mut stmt = db.prepare_cached("SELECT occurred_at_ms,kind FROM events WHERE attempt_id=?1 AND kind IN ('attempt.waiting','attempt.paused','attempt.resumed','attempt.succeeded','attempt.failed','attempt.interrupted') ORDER BY id")?;
    for row in stmt.query_map([attempt], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })? {
        let (at, kind) = row?;
        match kind.as_str() {
            "attempt.waiting" | "attempt.paused" => {
                started.get_or_insert(at);
            }
            _ => {
                if let Some(start) = started.take() {
                    total = total.saturating_add(at.saturating_sub(start));
                }
            }
        }
    }
    if let Some(start) = started {
        total = total.saturating_add(finished.unwrap_or_else(now_ms).saturating_sub(start));
    }
    Ok(total)
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

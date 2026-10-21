//! Runner-facing commands. Attempts own live state; clients request control through events.
use super::*;
use crate::storage::{execution::StartAttempt, usage::TokenUsage};
use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub id: i64,
    pub name: String,
    pub command: String,
    pub directory: String,
    pub timeout_ms: i64,
}
#[derive(Clone)]
pub struct Run {
    pub attempt: i64,
    pub task: i64,
    pub revision: i64,
    pub token: String,
    pub definition: Definition,
    pub active_ms: i64,
}
pub struct Step {
    pub id: i64,
    pub session: Option<i64>,
    pub provider_session: Option<String>,
}

impl Service {
    pub fn attempt_base(&self, task: i64) -> Result<Option<String>> {
        Ok(self.db.connection().query_row("SELECT base_commit FROM attempts WHERE project_id=?1 AND task_id=?2 ORDER BY number DESC LIMIT 1",params![self.project_id,task],|r|r.get(0)).optional()?.flatten())
    }
    pub fn supervisor_released(&self, attempt: i64) -> Result<bool> {
        let kind: Option<String> = self.db.connection().query_row(
            "SELECT kind FROM events WHERE attempt_id=?1 AND kind IN ('attempt.paused','attempt.waiting','supervisor.released') ORDER BY id DESC LIMIT 1",
            [attempt],
            |r| r.get(0),
        ).optional()?;
        Ok(kind.as_deref() == Some("supervisor.released"))
    }
    /// Record a deliberate handoff after the worker and its scratch have been cleaned.
    pub fn release_supervisor(&mut self, run: &Run) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.execute(
            "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
             SELECT ?1,?2,id,?4,'supervisor.released','info','Supervisor yielded this attempt for resume'
             FROM attempts WHERE id=?3 AND execution_token=?5 AND state IN ('paused','waiting')",
            params![self.project_id, run.task, run.attempt, now_ms(), run.token],
        )? == 1, "Attempt changed before supervisor handoff")?;
        tx.commit()?;
        Ok(())
    }
    pub fn recover_run(&mut self, expected: Option<VersionedTask>) -> Result<()> {
        let task = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
        if let Some(expected) = expected {
            require(
                task.id == expected.id && task.version == expected.version,
                "Current task changed; reopen Recover",
            )?;
        }
        require(
            matches!(task.status.as_str(), "running" | "paused" | "waiting"),
            "No abandoned attempt",
        )?;
        if matches!(task.status.as_str(), "paused" | "waiting") {
            let attempt = task.attempts.last().expect("live attempt has a row");
            require(
                !self.supervisor_released(attempt.id)?,
                "Supervisor yielded this attempt normally; resume or abandon it",
            )?;
        }
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        require(tx.execute(
            "UPDATE attempts SET state='interrupted', finished_at_ms=?3, failure_category='interrupted',
             outcome='Recovered abandoned supervisor; work and evidence retained',
             wait_reason=NULL, waiting_since_ms=NULL, resume_at_ms=NULL
             WHERE id=(SELECT latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2 AND row_version=?4)
             AND state IN ('running','paused','waiting')",
            params![self.project_id, task.id, now, task.version],
        )? == 1, "Attempt changed; refresh the task")?;
        tx.execute(
            "UPDATE questions SET state='cancelled', resolved_at_ms=?3
             WHERE attempt_id=(SELECT latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2)
             AND state='open'",
            params![self.project_id, task.id, now],
        )?;
        tx.execute(
            "UPDATE tasks SET row_version=row_version+1, updated_at_ms=?2 WHERE id=?1",
            params![task.id, now],
        )?;
        event(
            &tx,
            self.project_id,
            task.id,
            now,
            "attempt.interrupted",
            "Recovered abandoned supervisor; work and evidence retained",
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Abandon a paused or waiting attempt that nobody will resume. The caller must hold the
    /// runner lease so no live supervisor owns it. Interrupted attempts spend no budget.
    pub fn abandon_run(&mut self, target: VersionedTask, reason: &str) -> Result<()> {
        require(!reason.trim().is_empty(), "A reason is required")?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (status, attempt) = current_status(&tx, self.project_id, target)?;
        let attempt = attempt.filter(|_| matches!(status.as_str(), "paused" | "waiting"));
        let attempt = attempt.ok_or_else(|| {
            Error::Conflict(
                "Only paused or waiting current can be abandoned; refresh the task".into(),
            )
        })?;
        let now = now_ms();
        interrupt(&tx, self.project_id, target.id, attempt, now, reason)?;
        bump(&tx, target, now)?;
        tx.commit()?;
        Ok(())
    }
    /// Give up on current: any paused or waiting attempt is interrupted without spending
    /// budget, and the task moves to History as cancelled with its attempts and evidence.
    /// The caller must hold the runner lease.
    pub fn abandon_task(&mut self, target: VersionedTask, reason: &str) -> Result<()> {
        require(!reason.trim().is_empty(), "A reason is required")?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (status, attempt) = current_status(&tx, self.project_id, target)?;
        let now = now_ms();
        match (status.as_str(), attempt) {
            ("paused" | "waiting", Some(attempt)) => {
                interrupt(&tx, self.project_id, target.id, attempt, now, reason)?;
            }
            ("failed" | "interrupted" | "cancelled", _) => {}
            _ => {
                return Err(Error::Conflict(
                    "Only failed, paused or waiting current can be abandoned; refresh the task"
                        .into(),
                ));
            }
        }
        super::commands::close(
            &tx,
            self.project_id,
            &self.actor,
            target,
            "cancelled",
            reason,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn project_root(&self) -> Result<PathBuf> {
        let bytes = self.db.connection().query_row(
            "SELECT root_path FROM projects WHERE id = ?1",
            [self.project_id],
            |r| r.get::<_, Vec<u8>>(0),
        )?;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    pub fn current_task(&mut self) -> Result<Option<Task>> {
        let id = self
            .db
            .connection()
            .query_row(
                "SELECT id FROM current_tasks WHERE project_id = ?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.task(id)).transpose().map(Option::flatten)
    }
    pub fn checks(&self) -> Result<Vec<Check>> {
        Ok(self.db.connection().prepare("SELECT id, name, command, working_directory, timeout_ms FROM project_checks WHERE project_id = ?1 AND enabled = 1 ORDER BY id")?.query_map([self.project_id], |r| Ok(Check { id: r.get(0)?, name: r.get(1)?, command: r.get(2)?, directory: r.get(3)?, timeout_ms: r.get(4)? }))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn set_check(&mut self, name: &str, command: &str, timeout_ms: i64) -> Result<()> {
        require(
            !name.trim().is_empty() && !command.trim().is_empty() && timeout_ms > 0,
            "Check needs a name, command and positive timeout",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(!tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id = ?1 AND state IN ('running','paused','waiting'))", [self.project_id], |r| r.get::<_, bool>(0))?, "Cannot change checks during a live attempt")?;
        tx.execute("INSERT INTO project_checks(project_id,name,command,working_directory,timeout_ms,enabled) VALUES (?1,?2,?3,'.',?4,1) ON CONFLICT(project_id,name) DO UPDATE SET command=excluded.command, timeout_ms=excluded.timeout_ms, enabled=1", params![self.project_id,name,command,timeout_ms])?;
        tx.commit()?;
        Ok(())
    }
    pub fn set_default(&mut self, role: &str, model: &ModelConfig) -> Result<()> {
        require(
            ROLES.contains(&role) || role == "planning",
            "Unknown model role",
        )?;
        require(
            !model.provider.trim().is_empty()
                && !model.model.trim().is_empty()
                && model.effort.as_ref().is_none_or(|s| !s.trim().is_empty()),
            "Invalid model configuration",
        )?;
        self.db.connection().execute("INSERT INTO project_models(project_id,role,provider,model,effort) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(project_id,role) DO UPDATE SET provider=excluded.provider, model=excluded.model, effort=excluded.effort", params![self.project_id,role,model.provider,model.model,model.effort])?;
        Ok(())
    }
    /// Called only after the supervisor owns both project/workspace leases and passes preflight.
    pub fn begin_run(&mut self, target: VersionedTask, token: &str, base: &str) -> Result<Run> {
        let task = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
        require(
            task.id == target.id && task.version == target.version,
            "Current task changed before execution",
        )?;
        require(
            task.status != "failed" || self.open_question(task.id)?.is_none(),
            "Answer the owner question before retrying",
        )?;
        let resume = matches!(task.status.as_str(), "paused" | "waiting");
        let (attempt, revision, token, active_ms) = if resume {
            let (id, rev, saved, active) = self.db.connection().query_row("SELECT id,revision,execution_token,active_ms FROM attempts WHERE task_id=?1 AND state IN ('paused','waiting')", [task.id], |r| Ok((r.get(0)?,r.get(1)?,r.get::<_,String>(2)?,r.get(3)?)))?;
            self.db.resume_attempt(StartAttempt {
                task_id: task.id,
                expected_version: task.version,
                execution_token: &saved,
                now_ms: now_ms(),
            })?;
            (id, rev, saved, active)
        } else {
            let id = self.db.start_attempt_with_base(
                StartAttempt {
                    task_id: task.id,
                    expected_version: task.version,
                    execution_token: token,
                    now_ms: now_ms(),
                },
                Some(base),
            )?;
            (id, task.definition().number, token.to_owned(), 0)
        };
        let definition = task
            .revisions
            .iter()
            .find(|r| r.number == revision)
            .expect("attempt FK")
            .definition
            .clone();
        Ok(Run {
            attempt,
            task: task.id,
            revision,
            token,
            definition,
            active_ms,
        })
    }
    pub fn output(&self, run: &Run, stream: &str, text: &str) -> Result<()> {
        self.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) SELECT ?1,coalesce(max(sequence),0)+1,?2,?3,?4 FROM attempt_output WHERE attempt_id=?1", params![run.attempt,stream,now_ms(),text])?;
        Ok(())
    }
    pub fn output_cursor(&self, run: &Run) -> Result<i64> {
        Ok(self.db.connection().query_row(
            "SELECT coalesce(max(sequence),0) FROM attempt_output WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?)
    }
    pub fn write_output_since(
        &self,
        run: &Run,
        cursor: i64,
        writer: &mut impl std::io::Write,
    ) -> Result<()> {
        let mut query = self.db.connection().prepare(
            "SELECT content FROM attempt_output WHERE attempt_id=?1 AND sequence>?2 ORDER BY sequence",
        )?;
        let mut rows = query.query(params![run.attempt, cursor])?;
        while let Some(row) = rows.next()? {
            writer.write_all(row.get::<_, String>(0)?.as_bytes())?;
        }
        Ok(())
    }
    pub fn record_artifact(&self, run: &Run, name: &str, path: &str, size: i64) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,?2,'text/plain',?3,?4,?5)",
            params![run.attempt, name, path, size, now_ms()],
        )?;
        Ok(())
    }
    pub fn active_time(&self, run: &Run, active_ms: i64) -> Result<()> {
        require(self.db.connection().execute("UPDATE attempts SET active_ms=?2 WHERE id=?1 AND execution_token=?3 AND state='running' AND active_ms<=?2", params![run.attempt,active_ms,run.token])? == 1, "Runner lost ownership of attempt")
    }
    pub fn earlier_task_active_ms(&self, run: &Run) -> Result<i64> {
        Ok(self.db.connection().query_row(
            "SELECT coalesce(sum(active_ms),0) FROM attempts WHERE task_id=?1 AND id<>?2",
            params![run.task, run.attempt],
            |row| row.get(0),
        )?)
    }
    pub fn request_pause(&mut self, target: VersionedTask) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String,Option<i64>)> = tx.query_row("SELECT status,latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2 AND row_version=?3", params![self.project_id,target.id,target.version], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (status, latest) = current.ok_or_else(|| {
            Error::Conflict("Current task changed; refresh before pausing".into())
        })?;
        let session: Option<(String, String)> = tx
            .query_row(
                "SELECT mode,state FROM execution_sessions WHERE project_id=?1",
                [self.project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let active = session.as_ref().is_some_and(|(_, state)| {
            matches!(state.as_str(), "running" | "waiting" | "resume_requested")
        });
        let attempt = if matches!(status.as_str(), "running" | "waiting") {
            latest
        } else {
            None
        };
        require(
            attempt.is_some() || active,
            "Only an active current or queue handoff can pause",
        )?;
        let reason = if attempt.is_some() {
            "Pause requested; stopping worker"
        } else {
            "Pause requested; no later task will start"
        };
        tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'runner.pause','info',?5)", params![self.project_id,target.id,attempt,now_ms(),reason])?;
        tx.execute("UPDATE execution_sessions SET state='paused',reason=?2,updated_at_ms=?3 WHERE project_id=?1", params![self.project_id,reason,now_ms()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn control_cursor(&self, run: &Run) -> Result<i64> {
        self.pause_cursor(run.attempt)
    }
    pub fn pause_cursor(&self, attempt: i64) -> Result<i64> {
        Ok(self.db.connection().query_row(
            "SELECT coalesce(max(id),0) FROM events WHERE attempt_id=?1 AND kind='runner.pause'",
            [attempt],
            |r| r.get(0),
        )?)
    }
    pub fn open_question(&self, task: i64) -> Result<Option<(i64, String)>> {
        Ok(self.db.connection().query_row("SELECT q.id,q.prompt FROM questions q JOIN attempts a ON a.id=q.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 AND q.state='open' ORDER BY q.id DESC LIMIT 1", params![self.project_id,task], |r| Ok((r.get(0)?,r.get(1)?))).optional()?)
    }
    /// Park the attempt until the provider accepts work again. A named reset time is
    /// honoured (plus a minute of margin); otherwise back off 1, 2, 4, then every 8 minutes.
    /// Limits never fail the task or spend its budget.
    pub fn rate_wait(&mut self, run: &Run, reset_at_ms: Option<i64>) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let waits: i64 = tx.query_row(
            "SELECT rate_waits FROM attempts WHERE id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        let resume_at = match reset_at_ms {
            Some(at) => at.max(now) + 60_000,
            None => now + 60_000 * (1 << waits.min(3)),
        };
        let until = crate::clock::format_local(resume_at, now);
        let message = match reset_at_ms {
            Some(_) => {
                format!("Provider usage limit; resumes {until} with the same attempt and session")
            }
            None => {
                format!("Provider rate limited; retrying {until} with the same attempt and session")
            }
        };
        require(tx.execute("UPDATE attempts SET state='waiting',wait_reason='rate_limit',waiting_since_ms=?2,resume_at_ms=?5,rate_waits=min(rate_waits+1,3),report=?3 WHERE id=?1 AND execution_token=?4 AND state='running'",params![run.attempt,now,message,run.token,resume_at])?==1,"Attempt ownership lost during rate-limit wait")?;
        tx.execute("UPDATE attempt_steps SET state='interrupted',finished_at_ms=?2,report=?3 WHERE attempt_id=?1 AND state='running'",params![run.attempt,now,message])?;
        tx.execute("UPDATE model_calls SET state='failed',finished_at_ms=?2,error=?3 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1) AND state='running'",params![run.attempt,now,message])?;
        tx.execute(
            "UPDATE tasks SET row_version=row_version+1,updated_at_ms=?2 WHERE id=?1",
            params![run.task, now],
        )?;
        tx.execute(
            "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
             VALUES (?1,?2,?3,?4,'attempt.waiting','info',?5)",
            params![self.project_id, run.task, run.attempt, now, message],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Provider-limit waits since the last completed provider turn (capped at 3).
    pub fn rate_waits(&self, run: &Run) -> Result<i64> {
        Ok(self.db.connection().query_row(
            "SELECT rate_waits FROM attempts WHERE id=?1 AND execution_token=?2",
            params![run.attempt, run.token],
            |row| row.get(0),
        )?)
    }
    pub fn answer(&mut self, target: VersionedTask, answer: &str) -> Result<()> {
        require(!answer.trim().is_empty(), "An answer is required")?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.execute("UPDATE questions SET state='answered',answer=?4,answered_by=?5,resolved_at_ms=?6 WHERE state='open' AND attempt_id IN (SELECT latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2 AND row_version=?3 AND status IN ('waiting','failed'))", params![self.project_id,target.id,target.version,answer,self.actor.name,now_ms()])? > 0, "Question changed or task is not current")?;
        bump(&tx, target, now_ms())?;
        event(
            &tx,
            self.project_id,
            target.id,
            now_ms(),
            "question.answered",
            "Answer saved; resume current to continue",
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn answers(&self, run: &Run) -> Result<String> {
        Ok(self.db.connection().prepare("SELECT prompt || char(10) || answer FROM questions WHERE attempt_id=?1 AND state='answered' ORDER BY id")?.query_map([run.attempt], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?.join("\n"))
    }
    pub fn start_ai_step(
        &mut self,
        run: &Run,
        role: &str,
        model: &ModelConfig,
        scope: &str,
        nudge: bool,
    ) -> Result<Step> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(i64,Option<String>)> = tx.query_row("SELECT s.id,s.provider_session_id FROM ai_sessions s JOIN attempt_steps p ON p.session_id=s.id WHERE s.project_id=?1 AND s.task_id=?2 AND s.role=?3 AND s.provider=?4 AND s.provider_scope=?5 AND s.state='open' AND p.model=?6 AND p.effort IS ?7 ORDER BY p.id DESC LIMIT 1", params![self.project_id,run.task,role,model.provider,scope,model.model,model.effort], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (session, provider_session) = if let Some(saved) = previous {
            saved
        } else {
            tx.execute("INSERT INTO ai_sessions(project_id,task_id,role,provider,provider_scope,state,created_at_ms,last_used_at_ms) VALUES (?1,?2,?3,?4,?5,'open',?6,?6)", params![self.project_id,run.task,role,model.provider,scope,now_ms()])?;
            (tx.last_insert_rowid(), None)
        };
        tx.execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,session_id,provider,model,effort,invocation,started_at_ms) SELECT ?1,?2,?3,?4,coalesce(max(ordinal),0)+1,?5,'running',?6,?7,?8,?9,?10,?11 FROM attempt_steps WHERE attempt_id=?1", params![run.attempt,self.project_id,run.task,run.revision,role,session,model.provider,model.model,model.effort,if nudge {"nudge"} else if provider_session.is_some() {"resume"} else {"start"},now_ms()])?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO model_calls(project_id,task_id,revision,step_id,session_id,role,provider,model,effort,request_key,state,started_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'running',?11)", params![self.project_id,run.task,run.revision,id,session,role,model.provider,model.model,model.effort,format!("{}:{id}",run.token),now_ms()])?;
        tx.commit()?;
        Ok(Step {
            id,
            session: Some(session),
            provider_session,
        })
    }
    pub fn record_session(&self, step: &Step, session: &str) -> Result<()> {
        require(
            !session.trim().is_empty(),
            "Provider emitted an empty session ID",
        )?;
        self.db.connection().execute(
            "UPDATE ai_sessions SET provider_session_id=?2,last_used_at_ms=?3 WHERE id=?1",
            params![step.session, session, now_ms()],
        )?;
        Ok(())
    }
    pub fn record_usage(&self, step: &Step, usage: &TokenUsage) -> Result<()> {
        self.db.connection().execute("UPDATE model_calls SET input_tokens=?2,cache_read_tokens=?3,cache_write_tokens=?4,output_tokens=?5 WHERE step_id=?1", params![step.id,usage.input_tokens,usage.cache_read_tokens,usage.cache_write_tokens,usage.output_tokens])?;
        Ok(())
    }
    pub fn start_check(&self, run: &Run, check: &Check) -> Result<Step> {
        self.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,project_check_id,command,working_directory,timeout_ms,started_at_ms) SELECT ?1,?2,?3,?4,coalesce(max(ordinal),0)+1,'check','running',?5,?6,?7,?8,?9 FROM attempt_steps WHERE attempt_id=?1", params![run.attempt,self.project_id,run.task,run.revision,check.id,check.command,check.directory,check.timeout_ms,now_ms()])?;
        Ok(Step {
            id: self.db.connection().last_insert_rowid(),
            session: None,
            provider_session: None,
        })
    }
    pub fn record_check_counts(&self, step: &Step, counts: CheckCounts) -> Result<()> {
        require(
            self.db.connection().execute(
                "UPDATE attempt_steps SET passed_count=?2,failed_count=?3,ignored_count=?4
                 WHERE id=?1 AND role='check' AND state<>'running'",
                params![step.id, counts.passed, counts.failed, counts.ignored],
            )? == 1,
            "Check step changed before counts were recorded",
        )
    }
    pub fn previous_check_counts(&self, run: &Run, check: i64) -> Result<Option<CheckCounts>> {
        self.db
            .connection()
            .query_row(
                "SELECT s.passed_count,s.failed_count,s.ignored_count FROM attempt_steps s
             WHERE s.attempt_id=(SELECT previous.id FROM attempts previous
               WHERE previous.project_id=?1 AND previous.state='succeeded'
                 AND previous.head_commit=(SELECT base_commit FROM attempts WHERE id=?2)
               ORDER BY previous.id DESC LIMIT 1)
               AND s.project_check_id=?3 AND s.role='check' AND s.state='succeeded'
               ORDER BY s.id DESC LIMIT 1",
                params![self.project_id, run.attempt, check],
                |r| {
                    Ok(match r.get::<_, Option<i64>>(0)? {
                        Some(passed) => Some(CheckCounts {
                            passed,
                            failed: r.get(1)?,
                            ignored: r.get(2)?,
                        }),
                        None => None,
                    })
                },
            )
            .optional()
            .map(Option::flatten)
            .map_err(Into::into)
    }
    pub fn check_regressions(&self, run: &Run) -> Result<Vec<String>> {
        let mut regressions = Vec::new();
        for check in self.checks()? {
            let current = self
                .db
                .connection()
                .query_row(
                    "SELECT passed_count,failed_count,ignored_count FROM attempt_steps
                 WHERE attempt_id=?1 AND project_check_id=?2 AND state='succeeded'
                 ORDER BY id DESC LIMIT 1",
                    params![run.attempt, check.id],
                    |r| {
                        Ok(match r.get::<_, Option<i64>>(0)? {
                            Some(passed) => Some(CheckCounts {
                                passed,
                                failed: r.get(1)?,
                                ignored: r.get(2)?,
                            }),
                            None => None,
                        })
                    },
                )
                .optional()?
                .flatten();
            if let Some(previous) = self.previous_check_counts(run, check.id)? {
                let Some(current) = current else {
                    regressions.push(format!(
                        "Supervisor check {} reported no test counts; previous checkpoint recorded {} passed, {} failed, {} ignored, so test coverage cannot be validated",
                        check.name, previous.passed, previous.failed, previous.ignored
                    ));
                    continue;
                };
                if current.passed < previous.passed {
                    regressions.push(format!(
                        "Supervisor check {} passed tests dropped from {} at the previous checkpoint to {}",
                        check.name, previous.passed, current.passed
                    ));
                }
                if current.ignored > previous.ignored {
                    regressions.push(format!(
                        "Supervisor check {} ignored tests rose from {} at the previous checkpoint to {}",
                        check.name, previous.ignored, current.ignored
                    ));
                }
            }
        }
        Ok(regressions)
    }
    pub fn finish_step(
        &mut self,
        step: &Step,
        success: bool,
        report: &str,
        exit: Option<i32>,
    ) -> Result<()> {
        self.complete_step(
            step,
            if success { "succeeded" } else { "failed" },
            success,
            report,
            exit,
        )
    }
    pub fn finish_assessment(&mut self, step: &Step, needs_work: bool, report: &str) -> Result<()> {
        self.complete_step(
            step,
            if needs_work {
                "changes_requested"
            } else {
                "succeeded"
            },
            true,
            report,
            None,
        )
    }
    fn complete_step(
        &mut self,
        step: &Step,
        state: &str,
        success: bool,
        report: &str,
        exit: Option<i32>,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE attempt_steps SET state=?2,report=?3,finished_at_ms=?4,exit_code=CASE WHEN role='check' THEN ?5 ELSE NULL END WHERE id=?1 AND state='running'", params![step.id,state,report,now_ms(),exit])?;
        tx.execute("UPDATE model_calls SET state=?2,finished_at_ms=?3,error=?4 WHERE step_id=?1 AND state='running'", params![step.id,if success {"succeeded"} else {"failed"},now_ms(),if success {None} else {Some(report)}])?;
        if success {
            // A completed provider turn ends the provider-limit streak.
            tx.execute("UPDATE attempts SET rate_waits=0 WHERE id=(SELECT attempt_id FROM attempt_steps WHERE id=?1 AND role<>'check')", [step.id])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn criterion(
        &self,
        run: &Run,
        step: &Step,
        ordinal: i64,
        passed: bool,
        evidence: &str,
    ) -> Result<()> {
        self.db.connection().execute("INSERT INTO criterion_results(step_id,task_id,revision,criterion_ordinal,state,evidence) VALUES (?1,?2,?3,?4,?5,?6)", params![step.id,run.task,run.revision,ordinal,if passed {"passed"} else {"needs_work"},evidence])?;
        Ok(())
    }
    /// Finalize only after descendants are stopped/reaped and owned scratch is cleaned.
    pub fn finish_run(
        &mut self,
        run: &Run,
        state: &str,
        category: Option<&str>,
        report: &str,
        head: Option<&str>,
    ) -> Result<()> {
        require(
            matches!(
                state,
                "succeeded" | "failed" | "interrupted" | "paused" | "waiting"
            ),
            "Invalid runner result",
        )?;
        let live = matches!(state, "paused" | "waiting");
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        require(tx.execute("UPDATE attempts SET state=?2,finished_at_ms=?3,outcome=?4,failure_category=?5,report=?6,head_commit=?7,wait_reason=?8,waiting_since_ms=?9 WHERE id=?1 AND execution_token=?10 AND state='running'", params![run.attempt,state,(!live).then_some(now),(!live).then_some(report),if live {None} else {category},report,head,(state=="waiting").then_some("question"),(state=="waiting").then_some(now),run.token])? == 1, "Attempt is no longer owned by this runner")?;
        tx.execute("UPDATE attempt_steps SET state='interrupted',finished_at_ms=?2,report=?3 WHERE attempt_id=?1 AND state='running'",params![run.attempt,now,report])?;
        tx.execute("UPDATE model_calls SET state='interrupted',finished_at_ms=?2,error=?3 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1) AND state='running'",params![run.attempt,now,report])?;
        if state == "waiting" {
            tx.execute("INSERT INTO questions(attempt_id,prompt,state,created_at_ms) VALUES (?1,?2,'open',?3)",params![run.attempt,report,now])?;
        }
        if state == "succeeded" {
            tx.execute("UPDATE tasks SET lifecycle='succeeded',closed_at_ms=?2,resolved_by_kind='system',resolved_by='runner',resolution_reason=?3 WHERE id=?1",params![run.task,now,report])?;
        }
        tx.execute(
            "UPDATE tasks SET row_version=row_version+1,updated_at_ms=?2 WHERE id=?1",
            params![run.task, now],
        )?;
        tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,?5,'info',?6)",params![self.project_id,run.task,run.attempt,now,format!("attempt.{state}"),report])?;
        tx.commit()?;
        Ok(())
    }
}

/// Status and latest attempt of current, if it is this exact task version.
fn current_status(
    tx: &Transaction<'_>,
    project: i64,
    target: VersionedTask,
) -> Result<(String, Option<i64>)> {
    tx.query_row(
        "SELECT status, latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2 AND row_version=?3",
        params![project, target.id, target.version],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Conflict("Task is not current, or changed elsewhere; refresh the task".into()))
}

fn interrupt(
    tx: &Transaction<'_>,
    project: i64,
    task: i64,
    attempt: i64,
    now: i64,
    reason: &str,
) -> Result<()> {
    let report = format!("Abandoned: {reason}");
    require(tx.execute("UPDATE attempts SET state='interrupted',finished_at_ms=?2,outcome=?3,failure_category='interrupted',report=?3,wait_reason=NULL,waiting_since_ms=NULL,resume_at_ms=NULL WHERE id=?1 AND state IN ('paused','waiting')", params![attempt,now,report])? == 1, "Attempt changed; refresh the task")?;
    tx.execute("UPDATE questions SET state='cancelled',resolved_at_ms=?2 WHERE attempt_id=?1 AND state='open'", params![attempt,now])?;
    tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'attempt.interrupted','info',?5)", params![project,task,attempt,now,report])?;
    Ok(())
}

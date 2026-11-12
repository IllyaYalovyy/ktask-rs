//! Runner-facing commands. Attempts own live state; clients request control through events.
use super::commands::{allowed, labelled, reason_or};
use super::lifecycle::Action;
use super::{delivery::Stage, *};
use crate::storage::{execution::StartAttempt, usage::TokenUsage};
use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

/// Usage counters for attempts recorded before they existed, from their retained rows. New
/// attempts start with a zero row and triggers keep every row current, so after the one
/// backfill at open no command sums output content again.
const BACKFILL_OUTPUT_USAGE: &str = "INSERT INTO attempt_output_usage(attempt_id,bytes,rows) SELECT a.id,(SELECT coalesce(sum(length(CAST(o.content AS BLOB))),0) FROM attempt_output o WHERE o.attempt_id=a.id),(SELECT count(*) FROM attempt_output o WHERE o.attempt_id=a.id) FROM attempts a WHERE NOT EXISTS (SELECT 1 FROM attempt_output_usage u WHERE u.attempt_id=a.id) AND (?1 IS NULL OR a.id=?1)";

/// Backfill every attempt's counters once, in one write transaction, when storage opens.
pub(super) fn backfill_output_usage(db: &rusqlite::Connection) -> Result<()> {
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempts a WHERE NOT EXISTS (SELECT 1 FROM attempt_output_usage u WHERE u.attempt_id=a.id))",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    tx.execute(BACKFILL_OUTPUT_USAGE, [rusqlite::types::Null])?;
    tx.commit()?;
    Ok(())
}

fn reserve_output(tx: &Transaction<'_>, attempt: i64, bytes: u64, rows: u64) -> Result<()> {
    let byte_limit = crate::resource::limit("KTASK_OUTPUT_BYTES", 64 * 1024 * 1024);
    let row_limit = crate::resource::limit("KTASK_OUTPUT_ROWS", 100_000);
    // An attempt another binary recorded after this one opened has no counters yet.
    tx.execute(BACKFILL_OUTPUT_USAGE, [attempt])?;
    let (used_bytes, used_rows): (i64, i64) = tx.query_row(
        "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
        [attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if (used_bytes as u64).saturating_add(bytes) > byte_limit
        || (used_rows as u64).saturating_add(rows) > row_limit
    {
        return Err(Error::Conflict(format!(
            "Resource limit: attempt output exceeds {byte_limit} bytes or {row_limit} rows; captured output retained. Raise the limit or free capacity, then resume"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub id: i64,
    pub name: String,
    pub command: String,
    pub directory: String,
    pub timeout_ms: i64,
}
pub struct ReusableCheck {
    pub step: i64,
    pub attempt: i64,
    pub transcript: String,
    pub counts: Option<CheckCounts>,
}
#[derive(Clone)]
pub struct Run {
    pub attempt: i64,
    pub task: i64,
    pub revision: i64,
    pub token: String,
    pub definition: Definition,
    /// The category and templates pinned by the revision this attempt runs.
    pub category: Option<super::categories::Category>,
    pub active_ms: i64,
}
impl Run {
    pub fn runs_checks(&self) -> bool {
        self.category.as_ref().is_some_and(|c| c.checks)
    }
    /// The pinned category; a run restored without one takes its kind's built-in category.
    pub fn effective_category(&self) -> super::categories::Category {
        self.category
            .clone()
            .or_else(|| super::categories::Category::for_kind(self.definition.kind))
            .expect("only AI-run revisions are executed")
    }
}
pub struct Step {
    pub id: i64,
    pub session: Option<i64>,
    pub provider_session: Option<String>,
}
pub struct PendingControl {
    pub task: i64,
    pub attempt: i64,
    pub action: String,
    pub reason: String,
    pub actor: super::Actor,
    pub stash: Option<String>,
    pub stash_checked: bool,
    pub completed: bool,
}

impl Service {
    pub fn pending_control(&self) -> Result<Option<PendingControl>> {
        self.db.connection().query_row(
            "SELECT task_id,attempt_id,action,reason,actor_kind,actor,stash,state,stash_checked FROM pending_controls WHERE project_id=?1",
            [self.project_id], |r| {
                let kind: String = r.get(4)?;
                let state: String = r.get(7)?;
                Ok(PendingControl { task:r.get(0)?, attempt:r.get(1)?, action:r.get(2)?, reason:r.get(3)?, actor:super::Actor { kind:if kind=="ai" {super::ActorKind::Ai} else {super::ActorKind::Human}, name:r.get(5)? }, stash:r.get(6)?, completed:state=="completed", stash_checked:r.get::<_,i64>(8)? != 0 })
            }
        ).optional().map_err(Into::into)
    }
    pub fn record_control_stash(&mut self, attempt: i64, stash: Option<&str>) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.execute(
            "UPDATE pending_controls SET stash=?3,stash_checked=1 WHERE project_id=?1 AND attempt_id=?2 AND state='requested'",
            params![self.project_id,attempt,stash],
        )? == 1, "Control request changed while saving work")?;
        if let Some(label) = stash {
            let task: i64 = tx.query_row(
                "SELECT task_id FROM pending_controls WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )?;
            tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'task.stash','info',?5)", params![self.project_id,task,attempt,now_ms(),format!("Uncommitted changes saved as git stash \"{label}\"")])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn record_stash_event(&mut self, task: i64, attempt: i64, label: &str) -> Result<()> {
        self.db.connection().execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'task.stash','info',?5)", params![self.project_id,task,attempt,now_ms(),format!("Uncommitted changes saved as git stash \"{label}\"")])?;
        Ok(())
    }
    pub fn complete_control(&mut self, attempt: i64) -> Result<Option<String>> {
        let Some(control) = self.pending_control()? else {
            return Ok(None);
        };
        if control.attempt != attempt || control.completed {
            return Ok(None);
        }
        let task = self
            .task(control.task)?
            .ok_or_else(|| Error::Conflict("Controlled task disappeared".into()))?;
        let previous = std::mem::replace(&mut self.actor, control.actor);
        let result = if control.action == "stop" {
            if task.state() == super::lifecycle::State::Pending {
                Ok(())
            } else {
                self.stop_task(
                    VersionedTask {
                        id: task.id,
                        version: task.version,
                    },
                    &control.reason,
                )
            }
        } else if matches!(task.state(), super::lifecycle::State::Done) {
            Ok(())
        } else {
            self.mark_done(
                VersionedTask {
                    id: task.id,
                    version: task.version,
                },
                &control.reason,
            )
        };
        self.actor = previous;
        result?;
        let version = self
            .task(control.task)?
            .ok_or_else(|| Error::Conflict("Controlled task disappeared".into()))?
            .version;
        require(self.db.connection().execute(
            "UPDATE pending_controls SET state='completed',completed_version=?3 WHERE project_id=?1 AND attempt_id=?2 AND state='requested'",
            params![self.project_id,attempt,version],
        )? == 1, "Control request changed while completing")?;
        Ok(control.stash)
    }
    pub fn attempt_base(&self, task: i64) -> Result<Option<String>> {
        Ok(self.db.connection().query_row("SELECT base_commit FROM attempts WHERE project_id=?1 AND task_id=?2 ORDER BY number DESC LIMIT 1",params![self.project_id,task],|r|r.get(0)).optional()?.flatten())
    }
    /// The last attempt to start or resume owns checkout edits. A resumed attempt keeps
    /// its ID, so attempt creation order cannot identify the latest workspace handoff.
    /// Queue insertion and selection do not hand the workspace to an unstarted task.
    pub fn workspace_owner_attempt(&self) -> Result<Option<i64>> {
        self.db
            .connection()
            .query_row(
                "SELECT attempt_id FROM events WHERE project_id=?1 AND kind IN ('attempt.started','attempt.resumed') ORDER BY id DESC LIMIT 1",
                [self.project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
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
    /// A supervisor that died leaves its attempt marked running. Once the caller holds the
    /// workspace lease (so no supervisor or worker owns it), that attempt becomes Paused:
    /// Resume continues it from its recorded phase. False when nothing needed settling.
    pub fn settle_orphan(&mut self) -> Result<bool> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let orphan: Option<(i64, i64)> = tx
            .query_row(
                "SELECT a.id, a.task_id FROM attempts a WHERE a.project_id=?1
                 AND (a.state='running' OR (a.state='waiting' AND a.wait_reason<>'question'))",
                [self.project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((attempt, task)) = orphan else {
            return Ok(false);
        };
        let now = now_ms();
        let report = "The supervisor stopped unexpectedly; Resume continues this attempt";
        tx.execute("UPDATE attempts SET state='paused',report=?2,wait_reason=NULL,waiting_since_ms=NULL,resume_at_ms=NULL WHERE id=?1", params![attempt,report])?;
        close_unfinished(&tx, attempt, now, report, "interrupted")?;
        tx.execute(
            "UPDATE tasks SET row_version=row_version+1, updated_at_ms=max(updated_at_ms,?2) WHERE id=?1",
            params![task, now],
        )?;
        tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'attempt.paused','warning',?5)", params![self.project_id,task,attempt,now,report])?;
        tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'supervisor.released','info',?5)", params![self.project_id,task,attempt,now,report])?;
        tx.execute("UPDATE execution_sessions SET state='paused',reason=?2,updated_at_ms=?3 WHERE project_id=?1 AND state IN ('running','waiting','resume_requested')", params![self.project_id,report,now])?;
        tx.commit()?;
        Ok(true)
    }
    /// Stop: end the task's open attempt and return it to Pending; the next Run starts fresh
    /// with full limits. The caller holds the workspace lease, so no worker is alive, and has
    /// already saved the attempt's uncommitted changes.
    pub fn stop_task(&mut self, target: VersionedTask, reason: &str) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        allowed(&tx, self.project_id, target, Action::Stop)?;
        let attempt = latest_attempt(&tx, target.id)?;
        let now = now_ms();
        let reason = labelled(reason, "Stopped", &self.actor);
        end_attempt(&tx, self.project_id, target.id, attempt, now, &reason)?;
        tx.execute("UPDATE ai_sessions SET state='closed' WHERE project_id=?1 AND task_id=?2 AND state='open'", params![self.project_id,target.id])?;
        tx.execute(
            "UPDATE tasks SET reset_pending=1, reset_worktree=NULL WHERE id=?1",
            [target.id],
        )?;
        tx.execute("UPDATE execution_sessions SET state='stopped',reason=?2,updated_at_ms=?3 WHERE project_id=?1 AND EXISTS (SELECT 1 FROM current_tasks WHERE project_id=?1 AND id=?4)", params![self.project_id,reason,now,target.id])?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "task.stopped",
            "Back to Pending; the next Run starts fresh",
        )?;
        bump(&tx, target, now)?;
        tx.commit()?;
        Ok(())
    }
    /// A provider-limit wait that the owner paused becomes Paused; Resume tries at once.
    pub fn pause_wait(&mut self, run: &Run, reason: &str) -> Result<()> {
        let now = now_ms();
        require(self.db.connection().execute("UPDATE attempts SET state='paused',report=?3,wait_reason=NULL,waiting_since_ms=NULL,resume_at_ms=NULL WHERE id=?1 AND execution_token=?2 AND state='waiting'", params![run.attempt,run.token,reason])? == 1, "Attempt changed during its provider limit wait")?;
        self.db.connection().execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'attempt.paused','info',?5)", params![self.project_id,run.task,run.attempt,now,reason])?;
        Ok(())
    }
    /// The task was edited while stopped: its open attempt ends so the work continues in a
    /// new attempt with the new revision. The caller holds the workspace lease.
    pub fn end_for_new_revision(&mut self, target: VersionedTask) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = latest_attempt(&tx, target.id)?;
        let now = now_ms();
        end_attempt(
            &tx,
            self.project_id,
            target.id,
            attempt,
            now,
            "Task edited while stopped; the work continues in a new attempt with the new revision",
        )?;
        bump(&tx, target, now)?;
        tx.commit()?;
        Ok(())
    }
    /// Mark done from any unfinished state: the owner's judgment overrides the supervisor's.
    /// An open attempt ends first; the caller holds the workspace lease, as for Stop.
    pub fn mark_done(&mut self, target: VersionedTask, reason: &str) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        allowed(&tx, self.project_id, target, Action::MarkDone)?;
        let now = now_ms();
        let attempt = latest_attempt(&tx, target.id)?;
        let was_current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM current_tasks WHERE project_id=?1 AND id=?2)",
            params![self.project_id, target.id],
            |r| r.get(0),
        )?;
        end_attempt(
            &tx,
            self.project_id,
            target.id,
            attempt,
            now,
            &labelled(reason, "Marked done", &self.actor),
        )?;
        let reason = reason_or(reason, "Marked done", &self.actor);
        tx.execute("UPDATE ai_sessions SET state='closed' WHERE project_id=?1 AND task_id=?2 AND state='open'", params![self.project_id,target.id])?;
        let human: bool = tx.query_row(
            "SELECT kind='human' FROM task_details WHERE id=?1",
            [target.id],
            |r| r.get(0),
        )?;
        require(
            !human || !matches!(self.actor.kind, super::ActorKind::Ai),
            "Only the owner can mark Human work done",
        )?;
        super::commands::close(
            &tx,
            self.project_id,
            &self.actor,
            target,
            if human {
                "succeeded"
            } else {
                "succeeded_override"
            },
            &reason,
            now,
        )?;
        if was_current {
            tx.execute(
                "UPDATE execution_sessions SET state=CASE WHEN EXISTS(SELECT 1 FROM current_tasks WHERE project_id=?1) THEN 'stopped' ELSE 'completed' END,reason=?2,updated_at_ms=?3 WHERE project_id=?1 AND state='paused'",
                params![self.project_id,reason,now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Every registered project root in this instance, oldest first.
    pub fn project_roots(db: &Database) -> Result<Vec<PathBuf>> {
        let mut statement = db
            .connection()
            .prepare("SELECT root_path FROM projects ORDER BY id")?;
        let roots = statement
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .map(|bytes| Ok(PathBuf::from(OsString::from_vec(bytes?))))
            .collect::<Result<_>>()?;
        Ok(roots)
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
    /// The current task's id and when its latest attempt may resume, without loading the task:
    /// what a provider-limit wait polls.
    pub fn limit_wait(&self) -> Result<Option<(i64, Option<i64>)>> {
        Ok(self
            .db
            .connection()
            .prepare_cached("SELECT id, resume_at_ms FROM current_tasks WHERE project_id = ?1")?
            .query_row([self.project_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
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
    /// Sets this project's default for `role`, and with `new_projects` also the default that
    /// projects registered later start with.
    pub fn set_default(
        &mut self,
        role: &str,
        model: &ModelConfig,
        new_projects: bool,
    ) -> Result<()> {
        require(
            ROLES.contains(&role) || role == "planning",
            "Unknown model role",
        )?;
        self.validate_model(role, model)?;
        let tx = self.db.connection_mut().transaction()?;
        tx.execute("INSERT INTO project_models(project_id,role,provider,model,effort) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(project_id,role) DO UPDATE SET provider=excluded.provider, model=excluded.model, effort=excluded.effort", params![self.project_id,role,model.provider,model.model,model.effort])?;
        if new_projects {
            tx.execute("INSERT INTO default_models(role,provider,model,effort) VALUES (?1,?2,?3,?4) ON CONFLICT(role) DO UPDATE SET provider=excluded.provider, model=excluded.model, effort=excluded.effort", params![role,model.provider,model.model,model.effort])?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Called only after the supervisor owns both project/workspace leases and passes preflight.
    /// Start or resume current. `restart_limits` (Run and Retry) counts the task's attempt
    /// and time limits again from this attempt; the supervisor's own repairs keep counting.
    pub fn begin_run(
        &mut self,
        target: VersionedTask,
        token: &str,
        base: &str,
        restart_limits: bool,
    ) -> Result<Run> {
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
        let effective =
            if resume {
                let revision: i64 = self.db.connection().query_row(
                "SELECT revision FROM attempts WHERE task_id=?1 AND state IN ('paused','waiting')",
                [task.id], |row| row.get(0))?;
                &task
                    .revisions
                    .iter()
                    .find(|r| r.number == revision)
                    .expect("attempt FK")
                    .definition
            } else {
                &task.definition().definition
            };
        for (role, model) in ROLES.iter().zip(&effective.models) {
            let model = model.as_ref().ok_or_else(|| {
                Error::Conflict(format!("Configure {role} provider/model before running"))
            })?;
            self.validate_model(role, model)?;
        }
        let (attempt, revision, token, active_ms) = if resume {
            let (id, rev, saved, active) = self.db.connection().query_row("SELECT id,revision,execution_token,active_ms FROM attempts WHERE task_id=?1 AND state IN ('paused','waiting')", [task.id], |r| Ok((r.get(0)?,r.get(1)?,r.get::<_,String>(2)?,r.get(3)?)))?;
            self.db.resume_attempt(StartAttempt {
                task_id: task.id,
                expected_version: task.version,
                execution_token: &saved,
                now_ms: now_ms(),
            })?;
            // The owner may have committed on top while the attempt was paused; its own
            // change, and so its review, starts at the current HEAD. Delivery keeps its base.
            self.db.connection().execute(
                "UPDATE attempts SET base_commit=?2 WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM attempt_delivery WHERE attempt_id=?1 AND stage<>?3)",
                params![id, base, Stage::Work],
            )?;
            (id, rev, saved, active)
        } else {
            if restart_limits {
                self.db.connection().execute(
                    "UPDATE tasks SET counted_from=(SELECT count(*) FROM attempts WHERE task_id=?1) WHERE id=?1",
                    [task.id],
                )?;
            }
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
        let pinned = task
            .revisions
            .iter()
            .find(|r| r.number == revision)
            .expect("attempt FK");
        Ok(Run {
            attempt,
            task: task.id,
            revision,
            token,
            definition: pinned.definition.clone(),
            category: pinned.category.clone(),
            active_ms,
        })
    }
    pub fn output(&self, run: &Run, stream: &str, text: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        if let Err(error) = reserve_output(&tx, run.attempt, text.len() as u64, 1) {
            // Keep the one-time upgraded counter even when admission is denied.
            tx.commit()?;
            return Err(error);
        }
        if let Err(error) = super::retention::admit_capacity(
            &tx,
            super::retention::measure_known_bytes(&self.db)?,
            text.len() as u64,
            1,
            0,
            0,
        ) {
            tx.commit()?;
            return Err(error);
        }
        tx.execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) SELECT ?1,coalesce(max(sequence),0)+1,?2,?3,?4 FROM attempt_output WHERE attempt_id=?1", params![run.attempt,stream,now_ms(),text])?;
        tx.commit()?;
        Ok(())
    }
    /// Appends (stream, text) rows to the attempt's output in one transaction, so output that
    /// queued while other projects held the shared database costs one disk flush, not one each.
    pub fn output_rows(&mut self, run: &Run, rows: &[(String, String)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let measured = super::retention::measure_known_bytes(&self.db)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Err(error) = reserve_output(
            &tx,
            run.attempt,
            rows.iter().map(|(_, text)| text.len() as u64).sum(),
            rows.len() as u64,
        ) {
            tx.commit()?;
            return Err(error);
        }
        if let Err(error) = super::retention::admit_capacity(
            &tx,
            measured,
            rows.iter().map(|(_, text)| text.len() as u64).sum(),
            rows.len() as u64,
            0,
            0,
        ) {
            tx.commit()?;
            return Err(error);
        }
        let last: i64 = tx.query_row(
            "SELECT coalesce(max(sequence),0) FROM attempt_output WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        let now = now_ms();
        for (offset, (stream, text)) in rows.iter().enumerate() {
            tx.execute(
                "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,?2,?3,?4,?5)",
                params![run.attempt, last + 1 + offset as i64, stream, now, text],
            )?;
        }
        tx.commit()?;
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
    pub fn active_time(&self, run: &Run, active_ms: i64) -> Result<()> {
        require(self.db.connection().execute("UPDATE attempts SET active_ms=?2 WHERE id=?1 AND execution_token=?3 AND state='running' AND active_ms<=?2", params![run.attempt,active_ms,run.token])? == 1, "Runner lost ownership of attempt")
    }
    pub fn earlier_task_active_ms(&self, run: &Run) -> Result<i64> {
        Ok(self.db.connection().query_row(
            "SELECT coalesce(sum(a.active_ms),0) FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.task_id=?1 AND a.id<>?2 AND a.number>t.counted_from",
            params![run.task, run.attempt],
            |row| row.get(0),
        )?)
    }
    pub fn request_pause(&mut self, target: VersionedTask) -> Result<()> {
        self.request_pause_for(target, None)
    }
    /// Persist the owner's final control before signalling the runner. The runner completes
    /// this request under its workspace lease even if the requesting process disappears.
    pub fn request_control(
        &mut self,
        target: VersionedTask,
        action: &str,
        reason: &str,
    ) -> Result<()> {
        require(matches!(action, "stop" | "done"), "Invalid control action")?;
        self.request_pause_for(target, Some((action, reason)))
    }
    fn request_pause_for(
        &mut self,
        target: VersionedTask,
        control: Option<(&str, &str)>,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, Option<String>, Option<i64>)> = tx.query_row("SELECT status,wait_reason,latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2 AND row_version=?3", params![self.project_id,target.id,target.version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (status, wait_reason, latest) = current.ok_or_else(|| {
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
        if let Some((action, control_reason)) = control {
            require(
                lifecycle::State::of(&status, wait_reason.as_deref()) == lifecycle::State::Running
                    && attempt.is_some(),
                "Only a running task can defer a control",
            )?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT state FROM pending_controls WHERE project_id=?1",
                    [self.project_id],
                    |r| r.get(0),
                )
                .optional()?;
            require(
                existing.as_deref() != Some("requested"),
                "A Stop or Mark done is already pending",
            )?;
            tx.execute("INSERT INTO pending_controls(project_id,task_id,attempt_id,action,reason,actor_kind,actor,state) VALUES (?1,?2,?3,?4,?5,?6,?7,'requested') ON CONFLICT(project_id) DO UPDATE SET task_id=excluded.task_id,attempt_id=excluded.attempt_id,action=excluded.action,reason=excluded.reason,actor_kind=excluded.actor_kind,actor=excluded.actor,state='requested',stash=NULL,stash_checked=0,completed_version=NULL",
                params![self.project_id,target.id,attempt,action,control_reason,self.actor.kind.as_str(),self.actor.name])?;
        }
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
    /// A single worker-stop transition after this reset's pause request is the
    /// only version change reset may adopt from its original caller.
    pub fn verified_pause_after(&self, attempt: i64, pause_cursor: i64) -> Result<bool> {
        let paused: bool = self.db.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE attempt_id=?1 AND kind='attempt.paused' AND id>?2)",
            params![attempt,pause_cursor], |row| row.get(0))?;
        Ok(paused && self.supervisor_released(attempt)?)
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
        close_unfinished(&tx, run.attempt, now, &message, "failed")?;
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
    /// The owner chose to run now: a provider-limit wait on the current task ends early so the
    /// provider is asked again. A limit that still holds records a fresh wait.
    pub fn end_limit_wait(&mut self) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let ended = tx
            .query_row(
                "SELECT a.id,a.task_id FROM attempts a JOIN current_tasks t ON t.latest_attempt_id=a.id
                 WHERE t.project_id=?1 AND a.state='waiting' AND a.wait_reason='rate_limit' AND a.resume_at_ms>?2",
                params![self.project_id, now],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((attempt, task)) = ended {
            tx.execute(
                "UPDATE attempts SET resume_at_ms=?2, waiting_since_ms=min(waiting_since_ms,?2) WHERE id=?1",
                params![attempt, now],
            )?;
            tx.execute(
                "UPDATE tasks SET row_version=row_version+1,updated_at_ms=?2 WHERE id=?1",
                params![task, now],
            )?;
            tx.execute(
                "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
                 VALUES (?1,?2,?3,?4,'attempt.wait_ended','info','Owner resumed before the provider reset; asking the provider again')",
                params![self.project_id, task, attempt, now],
            )?;
        }
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
        allowed(&tx, self.project_id, target, Action::Answer)?;
        require(tx.execute("UPDATE questions SET state='answered',answer=?4,answered_by=?5,resolved_at_ms=?6 WHERE state='open' AND attempt_id IN (SELECT latest_attempt_id FROM current_tasks WHERE project_id=?1 AND id=?2 AND row_version=?3)", params![self.project_id,target.id,target.version,answer,self.actor.name,now_ms()])? > 0, "Question changed or task is not current")?;
        bump(&tx, target, now_ms())?;
        event(
            &tx,
            self.project_id,
            target.id,
            now_ms(),
            "question.answered",
            "Answer saved; the task continues",
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
    /// This step's share of a provider session's running totals: what the steps since the
    /// session started recorded is subtracted. A step that starts a new provider session
    /// reports its own usage; totals below the earlier steps mean the provider restarted.
    pub fn step_share(&self, step: &Step, totals: TokenUsage) -> Result<TokenUsage> {
        if step.provider_session.is_none() {
            return Ok(totals);
        }
        let earlier: (i64, i64, i64, i64) = self.db.connection().query_row(
            "SELECT coalesce(sum(coalesce(m.input_tokens,0)+coalesce(m.cache_read_tokens,0)+coalesce(m.cache_write_tokens,0)),0),
                    coalesce(sum(m.cache_read_tokens),0), coalesce(sum(m.cache_write_tokens),0), coalesce(sum(m.output_tokens),0)
             FROM model_calls m WHERE m.session_id=?1 AND m.step_id<?2
             AND m.step_id>=(SELECT coalesce(max(id),0) FROM attempt_steps WHERE session_id=?1 AND invocation='start' AND id<?2)",
            params![step.session, step.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let share = |now: Option<i64>, before: i64| now.map(|n| n - before);
        let (total, read, write, output) = (
            share(totals.total_input(), earlier.0),
            share(totals.cache_read_tokens, earlier.1),
            share(totals.cache_write_tokens, earlier.2),
            share(totals.output_tokens, earlier.3),
        );
        if step.session.is_none()
            || [total, read, write, output]
                .into_iter()
                .flatten()
                .any(|n| n < 0)
        {
            return Ok(totals);
        }
        TokenUsage::from_inclusive(total, read, write, output).or_else(|_| Ok(totals))
    }
    /// Totals so far for this step; providers that report per step resend running totals.
    pub fn record_usage(
        &self,
        step: &Step,
        usage: &TokenUsage,
        cost_microusd: Option<i64>,
    ) -> Result<()> {
        self.db.connection().execute("UPDATE model_calls SET input_tokens=?2,cache_read_tokens=?3,cache_write_tokens=?4,output_tokens=?5,cost_microusd=coalesce(?6,cost_microusd) WHERE step_id=?1", params![step.id,usage.input_tokens,usage.cache_read_tokens,usage.cache_write_tokens,usage.output_tokens,cost_microusd])?;
        Ok(())
    }
    pub fn start_check(&self, run: &Run, check: &Check) -> Result<Step> {
        self.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,project_check_id,check_name,command,working_directory,timeout_ms,started_at_ms) SELECT ?1,?2,?3,?4,coalesce(max(ordinal),0)+1,'check','running',?5,?6,?7,?8,?9,?10 FROM attempt_steps WHERE attempt_id=?1", params![run.attempt,self.project_id,run.task,run.revision,check.id,check.name,check.command,check.directory,check.timeout_ms,now_ms()])?;
        Ok(Step {
            id: self.db.connection().last_insert_rowid(),
            session: None,
            provider_session: None,
        })
    }
    pub fn reusable_check(&self, check: &Check, tree: &str) -> Result<Option<ReusableCheck>> {
        self.db.connection().query_row(
            "SELECT s.id,s.attempt_id,a.relative_path,s.passed_count,s.failed_count,s.ignored_count,s.state,s.exit_code
             FROM attempt_steps s LEFT JOIN artifacts a ON a.attempt_id=s.attempt_id
               AND a.relative_path='evidence/attempt-'||s.attempt_id||'/step-'||s.id||'.log'
               AND a.pruned_at_ms IS NULL AND a.pruning_requested_at_ms IS NULL
             WHERE s.project_id=?1 AND s.role='check' AND s.reused_from_step_id IS NULL
               AND s.project_check_id=?2 AND s.check_name=?3 AND s.command=?4
               AND s.working_directory=?5 AND s.timeout_ms=?6 AND s.source_tree=?7
               AND s.state IN ('succeeded','failed') ORDER BY s.id DESC LIMIT 1",
            params![self.project_id,check.id,check.name,check.command,check.directory,check.timeout_ms,tree],
            |r| {
                let state: String = r.get(6)?;
                let exit: Option<i32> = r.get(7)?;
                let failed: Option<i64> = r.get(4)?;
                let reusable = state == "succeeded" && exit == Some(0) && failed.unwrap_or(0) == 0;
                if !reusable {
                    return Ok(None);
                }
                let counts = match r.get::<_, Option<i64>>(3)? {
                    Some(passed) => Some(CheckCounts {
                        passed,
                        failed: r.get(4)?,
                        ignored: r.get(5)?,
                    }),
                    None => None,
                };
                Ok(Some(ReusableCheck {
                    step: r.get(0)?,
                    attempt: r.get(1)?,
                    transcript: r.get::<_,Option<String>>(2)?.unwrap_or_default(),
                    counts,
                }))
            },
        ).optional().map(Option::flatten).map_err(Into::into)
    }
    pub fn reuse_check(
        &self,
        run: &Run,
        check: &Check,
        tree: &str,
        source: &ReusableCheck,
        transcript: &std::path::Path,
    ) -> Result<()> {
        let report = format!(
            "Reused passing check from attempt #{} step #{} · transcript: {}",
            source.attempt,
            source.step,
            transcript.display()
        );
        require(self.db.connection().execute(
            "INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,project_check_id,check_name,command,working_directory,timeout_ms,source_tree,reused_from_step_id,exit_code,passed_count,failed_count,ignored_count,started_at_ms,finished_at_ms,report)
             SELECT ?1,?2,?3,?4,coalesce(max(ordinal),0)+1,'check','succeeded',?5,?6,?7,?8,?9,?10,?11,0,?12,?13,?14,?15,?15,?16 FROM attempt_steps WHERE attempt_id=?1",
            params![run.attempt,self.project_id,run.task,run.revision,check.id,check.name,check.command,check.directory,check.timeout_ms,tree,source.step,source.counts.map(|c|c.passed),source.counts.map(|c|c.failed),source.counts.map(|c|c.ignored),now_ms(),report],
        )? == 1, "Could not record reused check")
    }
    pub fn record_check_tree(&self, step: &Step, tree: &str) -> Result<()> {
        require(self.db.connection().execute("UPDATE attempt_steps SET source_tree=?2 WHERE id=?1 AND role='check' AND state IN ('succeeded','failed')", params![step.id,tree])? == 1, "Check result changed before recording its source tree")
    }
    /// A check's verdict, exit status and test counts are recorded together, once.
    pub fn finish_check(
        &self,
        step: &Step,
        passed: bool,
        report: &str,
        exit: Option<i32>,
        counts: Option<CheckCounts>,
    ) -> Result<()> {
        require(
            self.db.connection().execute(
                "UPDATE attempt_steps SET state=?2, report=?3, finished_at_ms=?4, exit_code=?5,
                    passed_count=?6, failed_count=?7, ignored_count=?8
                 WHERE id=?1 AND role='check' AND state='running'",
                params![
                    step.id,
                    if passed { "succeeded" } else { "failed" },
                    report,
                    now_ms(),
                    exit,
                    counts.map(|c| c.passed),
                    counts.map(|c| c.failed),
                    counts.map(|c| c.ignored),
                ],
            )? == 1,
            "Check step changed before its result was recorded",
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
    pub fn finish_step(&mut self, step: &Step, success: bool, report: &str) -> Result<()> {
        self.complete_step(
            step,
            if success { "succeeded" } else { "failed" },
            success,
            report,
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
        )
    }
    fn complete_step(
        &mut self,
        step: &Step,
        state: &str,
        success: bool,
        report: &str,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE attempt_steps SET state=?2,report=?3,finished_at_ms=?4 WHERE id=?1 AND state='running'", params![step.id,state,report,now_ms()])?;
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
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A Stop/Mark done accepted while the final step was ending wins over the
        // runner's success transition. Both decisions serialize in this transaction.
        let control_requested: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_controls WHERE project_id=?1 AND attempt_id=?2 AND state='requested')",
            params![self.project_id,run.attempt], |r| r.get(0),
        )?;
        let state = if state == "succeeded" && control_requested {
            "paused"
        } else {
            state
        };
        let live = matches!(state, "paused" | "waiting");
        let now = now_ms();
        require(tx.execute("UPDATE attempts SET state=?2,finished_at_ms=?3,outcome=?4,failure_category=?5,report=?6,head_commit=?7,wait_reason=?8,waiting_since_ms=?9 WHERE id=?1 AND execution_token=?10 AND state='running'", params![run.attempt,state,(!live).then_some(now),(!live).then_some(report),if live {None} else {category},report,head,(state=="waiting").then_some("question"),(state=="waiting").then_some(now),run.token])? == 1, "Attempt is no longer owned by this runner")?;
        close_unfinished(&tx, run.attempt, now, report, "interrupted")?;
        if matches!(state, "failed" | "interrupted") {
            // Claimable delivery ownership is committed with the terminal attempt state.
            // A later prerequisite insertion reroutes it in that same command's transaction.
            tx.execute("UPDATE attempt_delivery SET handoff_target=?2,repair_origin_task=coalesce(repair_origin_task,?2) WHERE attempt_id=?1 AND stage<>?3 AND handoff_claimed_by IS NULL",params![run.attempt,run.task,Stage::Work])?;
        }
        if state == "waiting" {
            tx.execute("INSERT INTO questions(attempt_id,prompt,state,created_at_ms) VALUES (?1,?2,'open',?3)",params![run.attempt,report,now])?;
        }
        if state == "succeeded" {
            tx.execute("UPDATE tasks SET lifecycle='succeeded',closed_at_ms=?2,resolved_by_kind='system',resolved_by='runner',resolution_reason=?3 WHERE id=?1",params![run.task,now,report])?;
            super::commands::lifecycle_event(
                &tx,
                run.task,
                "closed",
                Some("succeeded"),
                ("system", "runner"),
                report,
                now,
            )?;
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

/// An attempt that stops running takes its unfinished steps and provider calls with it, so
/// no record claims live work after its attempt ended. Finished steps stay as recorded.
fn close_unfinished(
    tx: &Transaction<'_>,
    attempt: i64,
    now: i64,
    reason: &str,
    call_state: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE model_calls SET state=?4, finished_at_ms=?2, error=?3
         WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1) AND state='running'",
        params![attempt, now, reason, call_state],
    )?;
    tx.execute(
        "UPDATE attempt_steps SET state='interrupted', finished_at_ms=?2, report=?3
         WHERE attempt_id=?1 AND state='running'",
        params![attempt, now, reason],
    )?;
    Ok(())
}

fn latest_attempt(tx: &Transaction<'_>, task: i64) -> Result<Option<(i64, String)>> {
    Ok(tx
        .query_row(
            "SELECT id, state FROM attempts WHERE task_id=?1 ORDER BY number DESC LIMIT 1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// End a stopped attempt (paused, or waiting for an answer) without spending any budget;
/// a finished attempt keeps its record. Open questions and unclaimed delivery work are
/// retired so nothing carries into the task's next Run.
fn end_attempt(
    tx: &Transaction<'_>,
    project: i64,
    task: i64,
    attempt: Option<(i64, String)>,
    now: i64,
    reason: &str,
) -> Result<()> {
    let Some((attempt, state)) = attempt else {
        return Ok(());
    };
    if matches!(state.as_str(), "running" | "paused" | "waiting") {
        require(tx.execute("UPDATE attempts SET state='interrupted',finished_at_ms=?2,outcome=?3,failure_category='interrupted',report=?3,wait_reason=NULL,waiting_since_ms=NULL,resume_at_ms=NULL WHERE id=?1 AND state IN ('paused','waiting')", params![attempt,now,reason])? == 1, "The task is still running; stop its worker first")?;
        close_unfinished(tx, attempt, now, reason, "interrupted")?;
        tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'attempt.interrupted','info',?5)", params![project,task,attempt,now,reason])?;
    }
    tx.execute("UPDATE questions SET state='cancelled',resolved_at_ms=?2 WHERE attempt_id=?1 AND state='open'", params![attempt,now])?;
    // Keep unclaimed verified delivery work, but make its successor recheck it against the
    // current source before publication.
    tx.execute("UPDATE attempt_delivery SET handoff_target=?2,repair_origin_task=coalesce(repair_origin_task,?2),restart_sync=1 WHERE attempt_id=?1 AND stage<>?3 AND handoff_claimed_by IS NULL", params![attempt,task,Stage::Work])?;
    Ok(())
}

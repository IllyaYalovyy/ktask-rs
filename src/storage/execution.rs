//! Transactional execution records only. No processes, scheduler, cleanup, or provider calls.
//! The future runner must satisfy workspace safety before recording a start.
use super::{Database, Error, Result};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

pub struct StartAttempt<'a> {
    pub task_id: i64,
    pub expected_version: i64,
    pub execution_token: &'a str,
    pub now_ms: i64,
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Conflict(message.into()))
    }
}

fn changed(tx: &Transaction<'_>, task: i64, now: i64, kind: &str, message: &str) -> Result<()> {
    tx.execute(
        "UPDATE tasks SET row_version = row_version + 1, updated_at_ms = ?2 WHERE id = ?1",
        params![task, now],
    )?;
    tx.execute("INSERT INTO events(project_id, task_id, attempt_id, occurred_at_ms, kind, level, message)
        SELECT project_id, id, latest_attempt_id, ?2, ?3, 'info', ?4 FROM task_details WHERE id = ?1",
        params![task, now, kind, message])?;
    Ok(())
}

impl Database {
    /// Records a start; it is NOT permission to spawn a worker or discard workspace changes.
    pub fn start_attempt(&mut self, command: StartAttempt<'_>) -> Result<i64> {
        self.start_attempt_with_base(command, None)
    }
    pub(crate) fn start_attempt_with_base(
        &mut self,
        command: StartAttempt<'_>,
        base: Option<&str>,
    ) -> Result<i64> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task: Option<(i64, i64, String)> = tx
            .query_row(
                "SELECT project_id, revision, kind FROM current_tasks
             WHERE id = ?1 AND row_version = ?2 AND lifecycle = 'ready'
               AND status IN ('ready', 'failed', 'interrupted', 'cancelled')
               AND revision_lifecycle = 'ready'
               AND (status='failed' OR attempt_limit IS NULL OR attempts_used < attempt_limit)",
                params![command.task_id, command.expected_version],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (project, revision, kind) = task.ok_or_else(|| {
            Error::Conflict(
                "task is stale, not current, not ready, or its attempt budget is exhausted".into(),
            )
        })?;
        require(!tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_sessions WHERE project_id=?1 AND state='paused')", [project], |r| r.get::<_,bool>(0))?, "Queue was paused before this attempt started")?;
        require(
            kind == "ai",
            "Human tasks require acknowledgment, not AI execution",
        )?;
        let configured: i64 = tx.query_row(
            "SELECT count(*) FROM revision_models WHERE task_id = ?1 AND revision = ?2",
            params![command.task_id, revision],
            |r| r.get(0),
        )?;
        require(
            configured == 4,
            "configure every task model role before execution",
        )?;
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM acceptance_criteria WHERE task_id = ?1 AND revision = ?2)",
            params![command.task_id, revision], |r| r.get(0))?, "task has no acceptance criteria")?;
        tx.execute(
            "INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token, started_at_ms, base_commit)
             VALUES (?1, ?2, ?3, (SELECT coalesce(max(number), 0) + 1 FROM attempts WHERE task_id = ?2), 'running', ?4, ?5, ?6)",
            params![project, command.task_id, revision, command.execution_token, command.now_ms,base])?;
        let id = tx.last_insert_rowid();
        changed(
            &tx,
            command.task_id,
            command.now_ms,
            "attempt.started",
            "Current task attempt recorded",
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Resumes the existing attempt once its timed wait is over; an owner ends it first.
    pub fn resume_attempt(&mut self, command: StartAttempt<'_>) -> Result<()> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(!tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_sessions WHERE project_id=(SELECT project_id FROM tasks WHERE id=?1) AND state='paused')", [command.task_id], |r| r.get::<_,bool>(0))?, "Queue was paused before this attempt resumed")?;
        let changed_rows = tx.execute(
            "UPDATE attempts SET state = 'running', wait_reason = NULL, waiting_since_ms = NULL, resume_at_ms = NULL
             WHERE task_id = ?1 AND execution_token = ?3 AND state IN ('waiting', 'paused')
               AND (waiting_since_ms IS NULL OR waiting_since_ms <= ?4)
               AND (resume_at_ms IS NULL OR resume_at_ms <= ?4)
               AND NOT EXISTS (SELECT 1 FROM questions WHERE attempt_id = attempts.id AND state = 'open')
               AND id IN (SELECT latest_attempt_id FROM current_tasks WHERE id = ?1 AND row_version = ?2)
               AND EXISTS (SELECT 1 FROM task_revisions r WHERE r.task_id = attempts.task_id
                   AND r.revision = attempts.revision
                   AND (r.attempt_timeout_ms IS NULL OR attempts.active_ms < r.attempt_timeout_ms))",
            params![command.task_id, command.expected_version, command.execution_token, command.now_ms])?;
        require(
            changed_rows == 1,
            "attempt is stale, not current, still waiting, or out of active time",
        )?;
        changed(
            &tx,
            command.task_id,
            command.now_ms,
            "attempt.resumed",
            "Existing attempt resumed",
        )?;
        tx.commit()?;
        Ok(())
    }
}

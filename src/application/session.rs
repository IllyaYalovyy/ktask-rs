//! Durable scheduler policy, separate from task and attempt lifecycles.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionSession {
    pub mode: String,
    pub state: String,
    pub reason: String,
}

impl Service {
    pub fn execution_session(&self) -> Result<Option<ExecutionSession>> {
        self.db
            .connection()
            .query_row(
                "SELECT mode,state,reason FROM execution_sessions WHERE project_id=?1",
                [self.project_id],
                |row| {
                    Ok(ExecutionSession {
                        mode: row.get(0)?,
                        state: row.get(1)?,
                        reason: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Explicit Run begins a new execution session. The caller must already own the
    /// workspace lease, so another window cannot replace a live supervisor's policy.
    pub fn start_execution_session(&mut self, queue: bool) -> Result<()> {
        let mode = if queue { "queue" } else { "one" };
        self.db.connection().execute(
            "INSERT INTO execution_sessions(project_id,mode,state,reason,updated_at_ms)
             VALUES (?1,?2,'running','Run requested',?3)
             ON CONFLICT(project_id) DO UPDATE SET mode=excluded.mode,state='running',reason='Run requested',updated_at_ms=excluded.updated_at_ms",
            params![self.project_id, mode, now_ms()],
        )?;
        Ok(())
    }

    /// Claim an intervention marker exactly once. A paused session must receive
    /// an explicit Run, including after a late CLI answer.
    pub fn resume_execution_session(&mut self) -> Result<bool> {
        Ok(self.db.connection().execute(
            "UPDATE execution_sessions SET state='running',reason='Intervention resolved; continuing queue',updated_at_ms=?2
             WHERE project_id=?1 AND mode='queue' AND state='resume_requested'",
            params![self.project_id, now_ms()],
        )? == 1)
    }

    /// Record the intervention before inspecting the supervisor lease. Either
    /// the exiting supervisor sees this marker, or the caller sees its released
    /// lease and claims the continuation itself.
    pub fn request_execution_continuation(&mut self) -> Result<()> {
        self.db.connection().execute(
            "UPDATE execution_sessions SET state='resume_requested',reason='Intervention resolved; waiting for supervisor handoff',updated_at_ms=?2
             WHERE project_id=?1 AND mode='queue' AND state IN ('running','waiting')
               AND reason!='Intervention resolved; continuing queue'
               AND EXISTS (SELECT 1 FROM current_tasks current
                   WHERE current.project_id=?1 AND current.kind='ai'
                     AND current.revision_lifecycle='ready'
                     AND current.status IN ('ready','failed','waiting','paused')
                     AND NOT EXISTS (SELECT 1 FROM questions question
                         WHERE question.attempt_id=current.latest_attempt_id
                           AND question.state='open'))",
            params![self.project_id, now_ms()],
        )?;
        Ok(())
    }

    /// The lease owner has taken responsibility for the next attempt. Clear a
    /// handoff marker it consumed itself, or the reservation used to launch it.
    pub fn supervisor_continuing_queue(&mut self) -> Result<()> {
        self.db.connection().execute(
            "UPDATE execution_sessions SET state='running',reason='Queue supervisor active',updated_at_ms=?2
             WHERE project_id=?1 AND mode='queue'
               AND (state='resume_requested'
                    OR (state='running' AND reason='Intervention resolved; continuing queue'))",
            params![self.project_id, now_ms()],
        )?;
        Ok(())
    }

    /// Retain a continuation error without replacing Pause or another Run.
    /// A handoff request stays claimable if its old supervisor is still alive.
    pub fn continuation_error(&mut self, reason: &str) -> Result<()> {
        require(!reason.trim().is_empty(), "Continuation error is required")?;
        self.db.connection().execute(
            "UPDATE execution_sessions SET reason=?2,updated_at_ms=?3
             WHERE project_id=?1 AND mode='queue'
               AND (state IN ('waiting','resume_requested')
                    OR (state='running' AND reason='Intervention resolved; continuing queue'))",
            params![self.project_id, reason, now_ms()],
        )?;
        Ok(())
    }

    /// A final Human acknowledgment or resolution can empty the queue without
    /// starting another supervisor. Do not overwrite Pause or newly added work.
    pub fn complete_execution_session_if_empty(&mut self) -> Result<()> {
        self.db.connection().execute(
            "UPDATE execution_sessions SET state='completed',reason='Queue complete after intervention',updated_at_ms=?2
             WHERE project_id=?1 AND mode='queue'
               AND state IN ('running','waiting','resume_requested')
               AND NOT EXISTS (SELECT 1 FROM current_tasks WHERE project_id=?1)",
            params![self.project_id, now_ms()],
        )?;
        Ok(())
    }

    /// Answering a Run one question never authorizes another automatic run.
    pub fn single_intervention_resolved(&mut self) -> Result<()> {
        self.db.connection().execute(
            "UPDATE execution_sessions
             SET state=CASE WHEN EXISTS(SELECT 1 FROM current_tasks WHERE project_id=?1)
                            THEN 'stopped' ELSE 'completed' END,
                 reason=CASE WHEN EXISTS(SELECT 1 FROM current_tasks WHERE project_id=?1)
                             THEN 'Intervention resolved; Run one awaits explicit Run or Resume'
                             ELSE 'Queue complete after intervention' END,
                 updated_at_ms=?2
             WHERE project_id=?1 AND mode='one'
               AND (state IN ('waiting','stopped')
                    OR (state='running' AND EXISTS (
                        SELECT 1 FROM current_tasks current
                        JOIN questions question ON question.attempt_id=current.latest_attempt_id
                        WHERE current.project_id=?1 AND current.status='waiting'
                          AND question.state='answered'))) ",
            params![self.project_id, now_ms()],
        )?;
        Ok(())
    }

    /// A failed launch may only return its own reservation to waiting. A
    /// concurrent explicit Run can replace it while the child starts.
    pub fn continuation_launch_failed(&mut self, reason: &str) -> Result<()> {
        self.db.connection().execute(
            "UPDATE execution_sessions SET state='waiting',reason=?2,updated_at_ms=?3
             WHERE project_id=?1 AND mode='queue' AND state='running'
               AND reason='Intervention resolved; continuing queue'",
            params![self.project_id, reason, now_ms()],
        )?;
        Ok(())
    }

    pub fn execution_state(&mut self, state: &str, reason: &str) -> Result<()> {
        require(
            matches!(
                state,
                "running" | "waiting" | "paused" | "stopped" | "completed"
            ),
            "Invalid execution state",
        )?;
        require(!reason.trim().is_empty(), "Execution reason is required")?;
        self.db.connection().execute(
            "UPDATE execution_sessions SET state=?2,reason=?3,updated_at_ms=?4
             WHERE project_id=?1 AND (state!='paused' OR ?2='paused')",
            params![self.project_id, state, reason, now_ms()],
        )?;
        Ok(())
    }

    /// Finish while the workspace lease is held. The intervention marker and
    /// final state serialize in one SQLite write transaction.
    pub fn settle_execution_session(&mut self, state: &str, reason: &str) -> Result<bool> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before: Option<(String, String)> = tx
            .query_row(
                "SELECT state,reason FROM execution_sessions WHERE project_id=?1",
                [self.project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let requested = before
            .as_ref()
            .is_some_and(|(state, _)| state == "resume_requested");
        let intervention_stopped = before.as_ref().is_some_and(|(state, reason)| {
            state == "stopped"
                && reason == "Intervention resolved; Run one awaits explicit Run or Resume"
        });
        if !matches!(
            before.as_ref().map(|(state, _)| state.as_str()),
            Some("paused" | "resume_requested" | "completed")
        ) && !intervention_stopped
        {
            tx.execute("UPDATE execution_sessions SET state=?2,reason=?3,updated_at_ms=?4 WHERE project_id=?1", params![self.project_id,state,reason,now_ms()])?;
        }
        tx.commit()?;
        Ok(requested)
    }
}

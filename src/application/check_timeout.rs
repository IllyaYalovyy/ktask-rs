//! An interrupted supervisor check and the decision that authorizes what follows it.
use super::{
    execution::{Check, Run, Step},
    progress::Decision,
    *,
};

const INCIDENT: &str =
    "SELECT s.id,s.check_name,s.project_check_id,s.timeout_ms,s.report,t.decision,t.reason,s.source_tree,t.output_start,t.output_end,s.command
    FROM check_timeouts t JOIN attempt_steps s ON s.id=t.step_id";

pub(crate) struct CheckTimeout {
    pub step: i64,
    pub name: String,
    pub check_id: i64,
    pub command: String,
    pub timeout_ms: i64,
    pub report: String,
    pub decision: Option<String>,
    pub reason: Option<String>,
    pub tree: String,
    pub output: (i64, i64),
}

pub(crate) struct TimeoutDecision<'a> {
    pub step: i64,
    pub action: &'static str,
    pub reason: &'a str,
    pub extension_ms: Option<i64>,
}

impl Decision for TimeoutDecision<'_> {
    fn record(&self, tx: &rusqlite::Transaction<'_>, run: &Run) -> Result<()> {
        require(
            tx.execute(
                "UPDATE check_timeouts SET decision=?3,reason=?4,extension_ms=?5
                 WHERE step_id=?1 AND (decision IS NULL OR decision='reconsider')
                   AND EXISTS (SELECT 1 FROM attempt_steps WHERE id=?1 AND attempt_id=?2)",
                params![
                    self.step,
                    run.attempt,
                    self.action,
                    self.reason,
                    self.extension_ms
                ],
            )? == 1,
            "Check timeout changed before its decision was recorded",
        )
    }
}

impl Service {
    pub(crate) fn record_check_timeout(
        &mut self,
        run: &Run,
        step: &Step,
        tree: &str,
        report: &str,
        (start, end): (i64, i64),
    ) -> Result<()> {
        let tx = self.db.connection_mut().transaction()?;
        require(
            tx.execute(
                "UPDATE attempt_steps SET state='failed',report=?4,source_tree=?5,finished_at_ms=?6
                 WHERE id=?1 AND attempt_id=?2 AND role='check' AND state='running'
                   AND EXISTS (SELECT 1 FROM attempts WHERE id=?2 AND execution_token=?3 AND state='running')",
                params![step.id,run.attempt,run.token,report,tree,now_ms()],
            )? == 1,
            "Check changed before its timeout was recorded",
        )?;
        tx.execute(
            "INSERT INTO check_timeouts(step_id,output_start,output_end) VALUES (?1,?2,?3)",
            params![step.id, start, end],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn pending_check_timeout(&self, run: &Run) -> Result<Option<CheckTimeout>> {
        self.db
            .connection()
            .query_row(
                &format!(
                    "{INCIDENT} WHERE s.attempt_id=?1
                AND (t.decision IS NULL OR t.decision IN ('reconsider','escalate'))
                ORDER BY s.id DESC LIMIT 1"
                ),
                [run.attempt],
                |r| {
                    Ok(CheckTimeout {
                        step: r.get(0)?,
                        name: r.get(1)?,
                        check_id: r.get(2)?,
                        timeout_ms: r.get(3)?,
                        report: r.get(4)?,
                        decision: r.get(5)?,
                        reason: r.get(6)?,
                        tree: r.get(7)?,
                        output: (r.get(8)?, r.get(9)?),
                        command: r.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn retained_check_transcript(
        &self,
        attempt: i64,
        root: &std::path::Path,
        relative: &str,
    ) -> Result<Option<std::path::PathBuf>> {
        let archived = self.db.connection().query_row(
            "SELECT EXISTS (SELECT 1 FROM artifacts WHERE attempt_id=?1 AND relative_path=?2
                            AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NULL)",
            params![attempt, relative],
            |r| r.get::<_, bool>(0),
        )?;
        Ok(archived
            .then(|| super::queries::retained_artifact_path(root, relative))
            .flatten())
    }

    pub(crate) fn complete_check_timeout_report(
        &self,
        run: &Run,
        timeout: &CheckTimeout,
        report: &str,
    ) -> Result<()> {
        require(self.db.connection().execute(
            "UPDATE attempt_steps SET report=?4 WHERE id=?1 AND attempt_id=?2
             AND EXISTS (SELECT 1 FROM check_timeouts WHERE step_id=?1)
             AND EXISTS (SELECT 1 FROM attempts WHERE id=?2 AND execution_token=?3 AND state='running')",
            params![timeout.step,run.attempt,run.token,report],
        )? == 1, "Check timeout changed before its full evidence was recorded")
    }

    /// Keep the original check settings and scope any larger limit to the current attempt.
    pub(crate) fn check_extension_for(&self, run: &Run, check_id: i64) -> Result<Option<i64>> {
        self.db
            .connection()
            .query_row(
                "SELECT t.extension_ms FROM check_timeouts t JOIN attempt_steps s ON s.id=t.step_id
             WHERE s.attempt_id=?1 AND s.project_check_id=?2 AND t.extension_ms IS NOT NULL",
                params![run.attempt, check_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// A completed repair must change the inputs before this exact invocation can run again.
    pub(crate) fn reconsider_check_timeout(
        &self,
        run: &Run,
        check: &Check,
        tree: &str,
        environment: &str,
    ) -> Result<bool> {
        Ok(self.db.connection().execute(
            "UPDATE check_timeouts SET decision='reconsider'
             WHERE step_id=(SELECT s.id FROM attempt_steps s JOIN check_timeouts t ON t.step_id=s.id
                 WHERE s.attempt_id=?1 AND s.project_check_id=?2 AND s.check_name=?3
                   AND s.command=?4 AND s.working_directory=?5 AND s.timeout_ms=?6 AND s.source_tree=?7
                   AND s.environment_fingerprint=?8 AND t.decision='repair' ORDER BY s.id DESC LIMIT 1)
               AND EXISTS (SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?9 AND state='running')",
            params![run.attempt,check.id,check.name,check.command,check.directory,check.timeout_ms,tree,environment,run.token],
        )? == 1)
    }
}

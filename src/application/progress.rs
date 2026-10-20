//! Persisted progress within an attempt; a review finding is ordinary work.
use super::{execution::Run, *};

pub const REPAIR_ROUNDS: i64 = 10;

#[derive(Clone, Debug)]
pub struct Progress {
    pub round: i64,
    pub phase: String,
    pub context: String,
    pub evidence: String,
    pub verified_tree: String,
}

impl Progress {
    pub fn label(&self) -> String {
        let phase = match self.phase.as_str() {
            "implement" => "Implement",
            "judge" => "Plan fixes",
            "fix" => "Fix",
            "check" => "Check",
            "review" => "Review",
            "validate" => "Validate",
            "decide" => "Critical findings need a decision",
            _ => "Complete",
        };
        format!(
            "Workflow · {phase} · repair round {}/{REPAIR_ROUNDS}",
            self.round
        )
    }
}

impl Service {
    pub fn progress(&self, run: &Run) -> Result<Option<Progress>> {
        self.db.connection().query_row(
            "SELECT repair_round,phase,context,evidence,verified_tree FROM attempt_progress WHERE attempt_id=?1",
            [run.attempt],
            read_progress,
        ).optional().map_err(Into::into)
    }

    pub fn prior_progress(&self, run: &Run) -> Result<Option<Progress>> {
        self.db
            .connection()
            .query_row(
                "SELECT p.repair_round,p.phase,p.context,p.evidence,p.verified_tree
             FROM attempt_progress p JOIN attempts a ON a.id=p.attempt_id
             WHERE a.task_id=?1 AND a.id<?2 ORDER BY a.id DESC LIMIT 1",
                params![run.task, run.attempt],
                read_progress,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Only the supervisor that applied a judge's replan can authorize its next worker.
    pub fn replan_applied(&self, run: &Run) -> Result<()> {
        require(self.db.connection().execute(
            "UPDATE attempt_progress SET phase='replanned' WHERE attempt_id=?1 AND phase='judge'
             AND EXISTS (SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='interrupted')",
            params![run.attempt,run.token],
        )? == 1, "Replanned attempt changed before handoff")
    }

    /// The runner owns this row through its attempt token, including during resume.
    pub fn save_progress(&mut self, run: &Run, progress: &Progress) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let label = progress.label();
        require(tx.execute(
            "UPDATE attempts SET report=?3 WHERE id=?1 AND execution_token=?2 AND state='running'",
            params![run.attempt,run.token,label],
        )? == 1, "Attempt ownership lost")?;
        tx.execute(
            "INSERT INTO attempt_progress(attempt_id,repair_round,phase,context,evidence,verified_tree)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(attempt_id) DO UPDATE SET repair_round=excluded.repair_round,
                 phase=excluded.phase,context=excluded.context,evidence=excluded.evidence,verified_tree=excluded.verified_tree",
            params![
                run.attempt,
                progress.round,
                progress.phase,
                progress.context,
                progress.evidence,
                progress.verified_tree
            ],
        )?;
        tx.execute(
            "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
             VALUES (?1,?2,?3,?4,'workflow.progress','info',?5)",
            params![self.project_id, run.task, run.attempt, now_ms(), label],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn read_progress(r: &rusqlite::Row<'_>) -> rusqlite::Result<Progress> {
    Ok(Progress {
        round: r.get(0)?,
        phase: r.get(1)?,
        context: r.get(2)?,
        evidence: r.get(3)?,
        verified_tree: r.get(4)?,
    })
}

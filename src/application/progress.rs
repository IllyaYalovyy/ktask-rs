//! Persisted progress within an attempt; a review finding is ordinary work.
use super::{execution::Run, *};

pub const REPAIR_ROUNDS: i64 = 10;

stored_names! {
    /// Where an attempt stands in its workflow (`attempt_progress.phase`).
    pub enum Phase {
        Baseline => "baseline",
        Implement => "implement",
        Judge => "judge",
        Replanned => "replanned",
        Fix => "fix",
        Integrate => "integrate",
        Check => "check",
        Review => "review",
        Validate => "validate",
        Decide => "decide",
        Complete => "complete",
    }
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub round: i64,
    pub phase: Phase,
    pub context: String,
    pub evidence: String,
    pub verified_tree: String,
    /// Why the current fix round runs, in one line the owner can read.
    pub why: String,
}

impl Progress {
    pub fn label(&self) -> String {
        let phase = match self.phase {
            Phase::Baseline => "Starting checks",
            Phase::Implement => "Implement",
            Phase::Judge => "Plan fixes",
            Phase::Fix => "Fix",
            Phase::Integrate => "Integrate remote changes",
            Phase::Check => "Check",
            Phase::Review => "Review",
            Phase::Validate => "Validate",
            Phase::Decide => "Critical findings need a decision",
            Phase::Replanned | Phase::Complete => "Complete",
        };
        let label = format!(
            "Workflow · {phase} · repair round {}/{REPAIR_ROUNDS}",
            self.round
        );
        match self.why.as_str() {
            "" => label,
            why => format!("{label}\nWhy: {why}"),
        }
    }
}

impl Service {
    pub fn progress(&self, run: &Run) -> Result<Option<Progress>> {
        self.db.connection().query_row(
            "SELECT repair_round,phase,context,evidence,verified_tree,why FROM attempt_progress WHERE attempt_id=?1",
            [run.attempt],
            read_progress,
        ).optional().map_err(Into::into)
    }

    pub fn prior_progress(&self, run: &Run) -> Result<Option<Progress>> {
        self.db
            .connection()
            .query_row(
                "SELECT p.repair_round,p.phase,p.context,p.evidence,p.verified_tree,p.why
             FROM attempt_progress p JOIN attempts a ON a.id=p.attempt_id
             LEFT JOIN attempt_delivery d ON d.attempt_id=a.id
             WHERE (a.task_id=?1 OR d.repair_origin_task=?1) AND a.id<?2 ORDER BY a.id DESC LIMIT 1",
                params![run.task, run.attempt],
                read_progress,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Whether the task's latest attempt ended in a judge's replan, so its next attempt is
    /// the supervisor's repair and keeps the task's repair-round count.
    pub fn replan_pending(&self, task: i64) -> Result<bool> {
        Ok(self.db.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM attempt_progress WHERE phase=?2 AND attempt_id=(SELECT max(id) FROM attempts WHERE task_id=?1))",
            params![task, Phase::Replanned],
            |r| r.get(0),
        )?)
    }

    /// Only the supervisor that applied a judge's replan can authorize its next worker.
    pub fn replan_applied(&self, run: &Run) -> Result<()> {
        require(self.db.connection().execute(
            "UPDATE attempt_progress SET phase=?3 WHERE attempt_id=?1 AND phase=?4
             AND EXISTS (SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='interrupted')",
            params![run.attempt,run.token,Phase::Replanned,Phase::Judge],
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
            "INSERT INTO attempt_progress(attempt_id,repair_round,phase,context,evidence,verified_tree,why)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(attempt_id) DO UPDATE SET repair_round=excluded.repair_round,
                 phase=excluded.phase,context=excluded.context,evidence=excluded.evidence,
                 verified_tree=excluded.verified_tree,why=excluded.why",
            params![
                run.attempt,
                progress.round,
                progress.phase,
                progress.context,
                progress.evidence,
                progress.verified_tree,
                progress.why
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
        why: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::Phase;

    #[test]
    fn every_phase_name_round_trips_and_is_allowed_by_the_schema() {
        let allowed = include_str!("../../migrations/0001_relational_state.sql")
            .lines()
            .find(|line| line.contains("phase TEXT NOT NULL CHECK"))
            .expect("phase constraint");
        for &phase in Phase::ALL {
            assert_eq!(Phase::parse(phase.as_str()), Some(phase));
            assert!(
                allowed.contains(&format!("'{phase}'")),
                "{phase} missing from schema"
            );
        }
        assert_eq!(allowed.matches('\'').count(), 2 * Phase::ALL.len());
        assert_eq!(Phase::parse("unknown"), None);
    }
}

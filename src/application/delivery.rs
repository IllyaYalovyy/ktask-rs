//! Durable delivery intent. The runner reconciles remote refs before repeating side effects.
use super::{configuration::DeliveryConfig, execution::Run, *};

#[derive(Clone, Debug)]
pub struct DeliveryAttempt {
    pub mode: String,
    pub remote: String,
    pub destination_url: String,
    pub base_branch: String,
    pub branch: Option<String>,
    pub stage: String,
    pub checkpoint_head: Option<String>,
    pub fetched_base: Option<String>,
    pub pushed_head: Option<String>,
    pub pr_url: Option<String>,
    pub retargeted: bool,
}

impl Service {
    pub fn delivery_pr_description(&self, run: &Run) -> Result<(String, String)> {
        let rows = self.db.connection().prepare("SELECT DISTINCT t.id,r.title,r.goal FROM attempt_delivery d JOIN attempts a ON a.id=d.attempt_id JOIN tasks t ON t.id=a.task_id JOIN task_revisions r ON r.task_id=a.task_id AND r.revision=a.revision WHERE a.project_id=?1 AND d.branch=(SELECT branch FROM attempt_delivery WHERE attempt_id=?2) AND (a.state='succeeded' OR a.id=?2) ORDER BY t.id")?.query_map(params![self.project_id,run.attempt],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let title = rows.first().map_or_else(
            || "Verified project changes".into(),
            |(_, title, _)| {
                if rows.len() == 1 {
                    title.clone()
                } else {
                    format!("{} (+{} tasks)", title, rows.len() - 1)
                }
            },
        );
        let mut body = String::from("Tasks included in this project branch:\n\n");
        for (id, title, goal) in rows {
            body.push_str(&format!("- #{id}: {title}\n  {goal}\n"));
        }
        body.push_str("\nVerification: required supervisor checks, independent code review, and independent validation passed for the published tree. Detailed records are retained in ktask RS.\n");
        Ok((title, body))
    }
    pub fn pending_delivery_handoff(&self, task: i64) -> Result<bool> {
        Ok(self.db.connection().query_row("SELECT EXISTS(SELECT 1 FROM attempt_delivery d JOIN attempts a ON a.id=d.attempt_id WHERE a.project_id=?1 AND d.handoff_target=?2 AND d.handoff_claimed_by IS NULL)",params![self.project_id,task],|r|r.get(0))?)
    }
    /// A delivery handed to this task by another task's attempt: an inserted prerequisite
    /// takes over the repaired task's uncommitted work. A task's own Stop hands the delivery
    /// back to itself and is not inherited work.
    pub fn inherits_delivery_handoff(&self, task: i64) -> Result<bool> {
        Ok(self.db.connection().query_row("SELECT EXISTS(SELECT 1 FROM attempt_delivery d JOIN attempts a ON a.id=d.attempt_id WHERE a.project_id=?1 AND d.handoff_target=?2 AND a.task_id<>?2 AND d.handoff_claimed_by IS NULL)",params![self.project_id,task],|r|r.get(0))?)
    }
    pub fn claim_delivery_handoff(
        &mut self,
        run: &Run,
        config: &DeliveryConfig,
        branch: Option<&str>,
        destination_url: &str,
    ) -> Result<bool> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source: Option<i64> = tx.query_row("SELECT d.attempt_id FROM attempt_delivery d JOIN attempts a ON a.id=d.attempt_id WHERE a.project_id=?1 AND d.handoff_target=?2 AND d.handoff_claimed_by IS NULL ORDER BY d.attempt_id DESC LIMIT 1",params![self.project_id,run.task],|r|r.get(0)).optional()?;
        let Some(source) = source else {
            return Ok(false);
        };
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='running')",params![run.attempt,run.token],|r|r.get(0))?,"Delivery successor ownership lost")?;
        let current: (i64, String, String, String) = tx.query_row("SELECT row_version,mode,remote,base_branch FROM project_delivery WHERE project_id=?1",[self.project_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.unwrap_or((0,"local".into(),"origin".into(),"main".into()));
        require(
            current
                == (
                    config.version,
                    config.mode.clone(),
                    config.remote.clone(),
                    config.base_branch.clone(),
                ),
            "Delivery configuration changed during handoff",
        )?;
        let prior: (String, String, String, String) = tx.query_row(
            "SELECT mode,remote,destination_url,base_branch FROM attempt_delivery WHERE attempt_id=?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if prior
            == (
                config.mode.clone(),
                config.remote.clone(),
                destination_url.into(),
                config.base_branch.clone(),
            )
        {
            // An unchanged target preserves the exact delivery state, including an
            // interrupted branch adoption that still needs to finish.
            tx.execute("INSERT INTO attempt_delivery(attempt_id,mode,remote,destination_url,base_branch,branch,stage,checkpoint_head,fetched_base,pushed_head,pr_url,pending_head,pending_tip,repair_origin_task,sync_cycles,retargeted) SELECT ?2,mode,remote,destination_url,base_branch,branch,stage,checkpoint_head,fetched_base,pushed_head,pr_url,pending_head,pending_tip,repair_origin_task,CASE WHEN restart_sync=1 THEN 0 ELSE sync_cycles END,retargeted FROM attempt_delivery WHERE attempt_id=?1",params![source,run.attempt])?;
        } else {
            // Keep the commit and task context, but discard the old destination's fetch,
            // push and PR state. A new proof is required before the new target is used.
            tx.execute("INSERT INTO attempt_delivery(attempt_id,mode,remote,destination_url,base_branch,branch,stage,checkpoint_head,repair_origin_task,sync_cycles,retargeted) SELECT ?2,?3,?4,?5,?6,?7,CASE WHEN checkpoint_head IS NOT NULL THEN 'checkpointed' WHEN ?3='local' THEN 'work' ELSE 'preparing' END,checkpoint_head,repair_origin_task,0,CASE WHEN checkpoint_head IS NOT NULL THEN 1 ELSE 0 END FROM attempt_delivery WHERE attempt_id=?1",params![source,run.attempt,config.mode,config.remote,destination_url,config.base_branch,branch])?;
        }
        tx.execute("UPDATE attempts SET base_commit=(SELECT base_commit FROM attempts WHERE id=?2) WHERE id=?1",params![run.attempt,source])?;
        // A Stop restarts from the checks at round 0 with no prior proof; an applied
        // replan keeps the repair round and evidence for its judge.
        tx.execute("INSERT INTO attempt_progress(attempt_id,repair_round,phase,context,evidence,verified_tree) SELECT ?2,CASE WHEN d.restart_sync=1 THEN 0 ELSE p.repair_round END,CASE WHEN d.restart_sync=1 THEN 'check' ELSE 'judge' END,p.context || char(10) || 'Retained Git delivery after explicit restart or applied replan; regenerate proof before publication.',CASE WHEN d.restart_sync=1 THEN '' ELSE p.evidence END,'' FROM attempt_progress p JOIN attempt_delivery d ON d.attempt_id=p.attempt_id WHERE p.attempt_id=?1 ON CONFLICT(attempt_id) DO NOTHING",params![source,run.attempt])?;
        tx.execute(
            "UPDATE attempt_delivery SET handoff_claimed_by=?2 WHERE attempt_id=?1",
            params![source, run.attempt],
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub fn current_delivery_stage(&self, task: i64) -> Result<Option<String>> {
        self.db.connection().query_row("SELECT d.stage FROM attempt_delivery d JOIN attempts a ON a.id=d.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 AND a.state IN ('running','paused','waiting')",params![self.project_id,task],|r|r.get(0)).optional().map_err(Into::into)
    }
    pub fn delivery_branch(&mut self, config: &DeliveryConfig) -> Result<String> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: Option<i64> = tx
            .query_row(
                "SELECT row_version FROM project_delivery WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?;
        require(
            version.unwrap_or(0) == config.version,
            "Delivery configuration changed during preflight",
        )?;
        let existing: Option<String> = tx.query_row("SELECT branch FROM project_delivery WHERE project_id=?1 AND mode='pr' AND remote=?2 AND base_branch=?3", params![self.project_id, config.remote, config.base_branch], |r| r.get(0)).optional()?.flatten();
        if let Some(branch) = existing {
            return Ok(branch);
        }
        let branch = format!(
            "ktask/project-{}-{}-{}",
            self.project_id,
            now_ms(),
            std::process::id()
        );
        tx.execute("INSERT INTO project_delivery(project_id,mode,remote,base_branch,branch) VALUES (?1,'pr',?2,?3,?4) ON CONFLICT(project_id) DO UPDATE SET branch=excluded.branch,owned_tip=NULL", params![self.project_id,config.remote,config.base_branch,branch])?;
        tx.commit()?;
        Ok(branch)
    }
    pub fn delivery_owned_tip(&self) -> Result<Option<String>> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT owned_tip FROM project_delivery WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }
    pub fn record_delivery_tip(
        &mut self,
        run: &Run,
        expected: Option<&str>,
        head: &str,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND project_id=?2 AND execution_token=?3 AND state='running')", params![run.attempt,self.project_id,run.token], |r| r.get::<_,bool>(0))?,"Delivery attempt ownership lost")?;
        require(
            tx.execute(
                "UPDATE project_delivery SET owned_tip=?3 WHERE project_id=?1 AND owned_tip IS ?2",
                params![self.project_id, expected, head],
            )? == 1,
            "Published branch ownership changed",
        )?;
        require(
            tx.execute(
                "UPDATE attempt_delivery SET pushed_head=?2,stage='pushed',pending_head=NULL,pending_tip=NULL WHERE attempt_id=?1",
                params![run.attempt, head],
            )? == 1,
            "Delivery attempt disappeared",
        )?;
        tx.commit().map_err(Into::into)
    }
    pub fn ensure_attempt_delivery(
        &mut self,
        run: &Run,
        config: &DeliveryConfig,
        branch: Option<&str>,
        destination_url: &str,
    ) -> Result<DeliveryAttempt> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND project_id=?2 AND execution_token=?3 AND state='running')", params![run.attempt,self.project_id,run.token], |r| r.get::<_,bool>(0))?,"Delivery attempt ownership lost")?;
        tx.execute("INSERT OR IGNORE INTO attempt_delivery(attempt_id,mode,remote,destination_url,base_branch,branch,stage) VALUES (?1,?2,?3,?4,?5,?6,CASE WHEN ?2='local' THEN 'work' ELSE 'preparing' END)", params![run.attempt,config.mode,config.remote,destination_url,config.base_branch,branch])?;
        tx.commit()?;
        self.attempt_delivery(run)
    }
    /// Reserve one distinct remote target before Git changes the checkout. Transport
    /// retries for that target and automatic ownership handoffs retain this count.
    pub fn begin_delivery_integration(&mut self, run: &Run, base: &str) -> Result<bool> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (cycles, previous): (i64, Option<String>) = tx.query_row("SELECT sync_cycles,fetched_base FROM attempt_delivery WHERE attempt_id=?1 AND EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='running')",params![run.attempt,run.token],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let fresh = previous.as_deref() != Some(base) || cycles == 0;
        if fresh && cycles >= 6 {
            return Ok(false);
        }
        tx.execute("UPDATE attempt_delivery SET stage='integrating',fetched_base=?2,sync_cycles=sync_cycles+?3 WHERE attempt_id=?1",params![run.attempt,base,i64::from(fresh)])?;
        tx.commit()?;
        Ok(true)
    }
    pub fn reset_delivery_verification(
        &mut self,
        run: &Run,
        head: &str,
        reason: &str,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.execute("UPDATE attempt_delivery SET stage='checkpointed',checkpoint_head=?3 WHERE attempt_id=?1 AND EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='running')",params![run.attempt,run.token,head])? == 1,"Delivery ownership lost")?;
        require(tx.execute("UPDATE attempt_progress SET phase='check',verified_tree='',evidence=context || char(10) || ?2 WHERE attempt_id=?1",params![run.attempt,reason])? == 1,"Delivery proof missing")?;
        tx.commit().map_err(Into::into)
    }
    pub fn delivery_push_intent(&self, run: &Run, head: &str, expected: &str) -> Result<()> {
        require(self.db.connection().execute("UPDATE attempt_delivery SET pending_head=?3,pending_tip=?4 WHERE attempt_id=?1 AND EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='running')",params![run.attempt,run.token,head,expected])? == 1,"Delivery ownership lost")
    }
    pub fn pending_delivery_push(&self, run: &Run) -> Result<Option<(String, String)>> {
        Ok(self.db.connection().query_row("SELECT pending_head,pending_tip FROM attempt_delivery WHERE attempt_id=?1 AND pending_head IS NOT NULL",[run.attempt],|r|Ok((r.get(0)?,r.get(1)?))).optional()?)
    }
    pub fn finish_delivery_prepare(&mut self, run: &Run, base: &str) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.execute("UPDATE attempts SET base_commit=?3 WHERE id=?1 AND execution_token=?2 AND state='running' AND EXISTS(SELECT 1 FROM attempt_delivery WHERE attempt_id=?1 AND stage='preparing')", params![run.attempt,run.token,base])? == 1, "Delivery preparation ownership lost")?;
        tx.execute(
            "UPDATE attempt_delivery SET stage='work',checkpoint_head=NULL WHERE attempt_id=?1",
            [run.attempt],
        )?;
        tx.commit().map_err(Into::into)
    }
    pub fn attempt_delivery(&self, run: &Run) -> Result<DeliveryAttempt> {
        self.db.connection().query_row("SELECT mode,remote,destination_url,base_branch,branch,stage,checkpoint_head,fetched_base,pushed_head,pr_url,retargeted FROM attempt_delivery WHERE attempt_id=?1", [run.attempt], |r| Ok(DeliveryAttempt { mode:r.get(0)?, remote:r.get(1)?, destination_url:r.get(2)?, base_branch:r.get(3)?, branch:r.get(4)?, stage:r.get(5)?, checkpoint_head:r.get(6)?, fetched_base:r.get(7)?, pushed_head:r.get(8)?, pr_url:r.get(9)?, retargeted:r.get(10)? })).map_err(Into::into)
    }
    /// Complete adoption only after Git is on the intended checkpoint and branch.
    /// An interrupted Git switch leaves this marker set for the next supervisor.
    pub fn complete_delivery_retarget(&self, run: &Run) -> Result<()> {
        require(self.db.connection().execute(
            "UPDATE attempt_delivery SET retargeted=0 WHERE attempt_id=?1 AND retargeted=1 AND stage='checkpointed' AND EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND project_id=?2 AND execution_token=?3 AND state='running')",
            params![run.attempt, self.project_id, run.token],
        )? == 1, "Delivery adoption ownership lost")
    }
    pub fn delivery_stage(
        &mut self,
        run: &Run,
        stage: &str,
        checkpoint: Option<&str>,
        base: Option<&str>,
        pr_url: Option<&str>,
    ) -> Result<()> {
        require(
            matches!(
                stage,
                "preparing"
                    | "work"
                    | "checkpointing"
                    | "checkpointed"
                    | "integrating"
                    | "verified"
                    | "pushed"
                    | "pr_ready"
            ),
            "Invalid delivery stage",
        )?;
        require(self.db.connection().execute("UPDATE attempt_delivery SET stage=?2,checkpoint_head=coalesce(?3,checkpoint_head),fetched_base=coalesce(?4,fetched_base),pr_url=coalesce(?5,pr_url) WHERE attempt_id=?1 AND EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?6 AND state='running')", params![run.attempt,stage,checkpoint,base,pr_url,run.token])? == 1, "Delivery attempt ownership lost")?;
        self.db.connection().execute(
            "UPDATE attempts SET report=?3 WHERE id=?1 AND execution_token=?2 AND state='running'",
            params![run.attempt, run.token, format!("Git delivery · {stage}")],
        )?;
        Ok(())
    }
}

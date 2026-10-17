use super::*;
use crate::storage::ordering;

fn checked(tx: &Transaction<'_>, project: i64, target: VersionedTask) -> Result<(String, String)> {
    tx.query_row("SELECT lifecycle, status FROM task_details WHERE project_id = ?1 AND id = ?2 AND row_version = ?3",
        params![project, target.id, target.version], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
        .ok_or_else(|| Error::Conflict("Task changed elsewhere; cancel and reopen this form".into()))
}
fn editable(lifecycle: &str, status: &str) -> Result<()> {
    require(
        matches!(lifecycle, "new" | "ready")
            && matches!(
                status,
                "new" | "ready" | "failed" | "interrupted" | "cancelled"
            ),
        "Task is read-only while live or resolved",
    )
}
fn insert_revision(
    tx: &Transaction<'_>,
    task: i64,
    number: i64,
    definition: &Definition,
    ready: bool,
    now: i64,
    actor: &Actor,
) -> Result<()> {
    definition.validate()?;
    tx.execute("INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by, attempt_limit, attempt_timeout_ms, created_at_ms, decided_at_ms, decided_by_kind, decided_by)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![task, number, if ready { "ready" } else { "draft" }, definition.kind.as_str(), definition.title, definition.goal, actor.kind.as_str(), actor.name, definition.attempt_limit, definition.timeout_ms, now,
            ready.then_some(now), ready.then_some(actor.kind.as_str()), ready.then_some(&actor.name)])?;
    for (index, criterion) in definition.criteria.iter().enumerate() {
        tx.execute("INSERT INTO acceptance_criteria(task_id, revision, ordinal, description) VALUES (?1, ?2, ?3, ?4)", params![task, number, index as i64 + 1, criterion])?;
    }
    for (role, model) in ROLES.iter().zip(&definition.models) {
        if let Some(model) = model {
            tx.execute("INSERT INTO revision_models(task_id, revision, role, provider, model, effort) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![task, number, role, model.provider, model.model, model.effort])?;
        }
    }
    Ok(())
}

impl Service {
    /// Apply a validated remediation proposal atomically to failed current.
    pub fn apply_remediation(
        &mut self,
        target: VersionedTask,
        model: Option<ModelConfig>,
        repairs: &[Definition],
        reason: &str,
        ready: bool,
    ) -> Result<Vec<i64>> {
        require(
            !reason.trim().is_empty(),
            "A remediation reason is required",
        )?;
        require(
            model.is_some() ^ !repairs.is_empty(),
            "Choose one remediation strategy",
        )?;
        let existing = if model.is_some() {
            Some(
                self.task(target.id)?
                    .ok_or_else(|| Error::Conflict("Task disappeared".into()))?
                    .definition()
                    .definition
                    .clone(),
            )
        } else {
            None
        };
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (lifecycle, status) = checked(&tx, self.project_id, target)?;
        require(
            lifecycle == "ready" && status == "failed",
            "Only failed current can be remediated",
        )?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT id FROM current_tasks WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?;
        require(
            current == Some(target.id),
            "Only failed current can be remediated",
        )?;
        let now = now_ms();
        let remediator = Actor {
            kind: ActorKind::Ai,
            name: "remediator".into(),
        };
        let mut ids = Vec::new();
        if let Some(model) = model {
            let mut definition = existing.expect("escalation definition loaded");
            require(
                definition.models[0].as_ref() != Some(&model),
                "Escalation must change the execution model or effort",
            )?;
            definition.models[0] = Some(model);
            let number: i64 = tx.query_row(
                "SELECT max(revision)+1 FROM task_revisions WHERE task_id=?1",
                [target.id],
                |r| r.get(0),
            )?;
            insert_revision(&tx, target.id, number, &definition, true, now, &remediator)?;
            tx.execute(
                "UPDATE task_revisions SET change_reason=?3 WHERE task_id=?1 AND revision=?2",
                params![target.id, number, reason],
            )?;
            bump(&tx, target, now)?;
            event(
                &tx,
                self.project_id,
                target.id,
                now,
                "remediation.escalated",
                reason,
            )?;
            ids.push(target.id);
        } else {
            for definition in repairs {
                definition.validate()?;
            }
            let keys = ordering::keys_before(&tx, self.project_id, Some(target.id), repairs.len())?;
            for (definition, key) in repairs.iter().zip(keys) {
                tx.execute("INSERT INTO tasks(project_id,order_key,lifecycle,origin,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'remediator',?4,?4)", params![self.project_id,key,if ready {"ready"} else {"new"},now])?;
                let id = tx.last_insert_rowid();
                insert_revision(&tx, id, 1, definition, ready, now, &remediator)?;
                tx.execute(
                    "UPDATE task_revisions SET change_reason=?2 WHERE task_id=?1 AND revision=1",
                    params![id, reason],
                )?;
                event(
                    &tx,
                    self.project_id,
                    id,
                    now,
                    "task.remediation_inserted",
                    reason,
                )?;
                ids.push(id);
            }
        }
        tx.commit()?;
        Ok(ids)
    }
    /// Atomic batch command, also used for imports. No partially imported queue on failure.
    pub fn create(
        &mut self,
        definitions: &[Definition],
        before: Option<i64>,
        ready: bool,
    ) -> Result<Vec<i64>> {
        self.create_with_origin(
            definitions,
            before,
            ready,
            match self.actor.kind {
                ActorKind::Human => "human",
                ActorKind::Ai => "planner",
            },
        )
    }
    pub fn import(&mut self, definitions: &[Definition], before: Option<i64>) -> Result<Vec<i64>> {
        self.create_with_origin(definitions, before, true, "import")
    }
    fn create_with_origin(
        &mut self,
        definitions: &[Definition],
        before: Option<i64>,
        ready: bool,
        origin: &str,
    ) -> Result<Vec<i64>> {
        require(!definitions.is_empty(), "No tasks to create")?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let keys = ordering::keys_before(&tx, self.project_id, before, definitions.len())?;
        let now = now_ms();
        let mut ids = Vec::new();
        for (definition, key) in definitions.iter().zip(keys) {
            tx.execute("INSERT INTO tasks(project_id, order_key, lifecycle, origin, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?5, ?4, ?4)", params![self.project_id, key, if ready { "ready" } else { "new" }, now, origin])?;
            let id = tx.last_insert_rowid();
            insert_revision(&tx, id, 1, definition, ready, now, &self.actor)?;
            event(
                &tx,
                self.project_id,
                id,
                now,
                "task.created",
                &format!("#{id} created: {}", definition.title),
            )?;
            ids.push(id);
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn revise(
        &mut self,
        target: VersionedTask,
        definition: &Definition,
        ready: bool,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (lifecycle, status) = checked(&tx, self.project_id, target)?;
        editable(&lifecycle, &status)?;
        let (number, old_kind): (i64, String) = tx.query_row(
            "SELECT revision + 1, kind FROM task_revisions WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
            [target.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        require(
            !matches!(status.as_str(), "failed" | "interrupted" | "cancelled")
                || old_kind == definition.kind.as_str(),
            "Keep the kind of an attempted task",
        )?;
        let now = now_ms();
        insert_revision(&tx, target.id, number, definition, ready, now, &self.actor)?;
        if ready {
            tx.execute(
                "UPDATE tasks SET lifecycle = 'ready' WHERE id = ?1",
                [target.id],
            )?;
        }
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "revision.created",
            &format!("#{} revision {number} saved", target.id),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn decide(&mut self, target: VersionedTask, approve: bool, reason: &str) -> Result<()> {
        require(approve || !reason.trim().is_empty(), "A reason is required")?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (lifecycle, status) = checked(&tx, self.project_id, target)?;
        editable(&lifecycle, &status)?;
        let revision: Option<i64> = tx.query_row(
            "SELECT pending_revision FROM task_details WHERE id = ?1",
            [target.id],
            |r| r.get(0),
        )?;
        let revision = revision.ok_or_else(|| Error::Conflict("No pending revision".into()))?;
        let now = now_ms();
        tx.execute("UPDATE task_revisions SET lifecycle = ?3, decided_at_ms = max(created_at_ms, ?4), decided_by_kind = ?6, decided_by = ?7, change_reason = ?5 WHERE task_id = ?1 AND revision = ?2",
            params![target.id, revision, if approve { "ready" } else { "rejected" }, now, reason, self.actor.kind.as_str(), self.actor.name])?;
        let has_ready: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM task_revisions WHERE task_id = ?1 AND lifecycle = 'ready')", [target.id], |r| r.get(0))?;
        if has_ready {
            tx.execute(
                "UPDATE tasks SET lifecycle = 'ready' WHERE id = ?1",
                [target.id],
            )?;
        } else {
            tx.execute("UPDATE tasks SET lifecycle = 'cancelled', closed_at_ms = max(created_at_ms, ?2), resolution_reason = ?3, resolved_by_kind = ?4, resolved_by = ?5 WHERE id = ?1", params![target.id, now, reason, self.actor.kind.as_str(), self.actor.name])?;
        }
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "revision.decided",
            &format!(
                "#{} revision {revision} {} by {}: {}",
                target.id,
                if approve { "approved" } else { "rejected" },
                self.actor.kind.as_str(),
                self.actor.name
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn move_before(&mut self, target: VersionedTask, before: Option<i64>) -> Result<()> {
        require(
            before != Some(target.id),
            "Cannot move a task before itself",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, status) = checked(&tx, self.project_id, target)?;
        require(
            matches!(status.as_str(), "ready" | "new"),
            "Only pending work can move",
        )?;
        let key = ordering::keys_before(&tx, self.project_id, before, 1)?.remove(0);
        tx.execute(
            "UPDATE tasks SET order_key = ?2 WHERE id = ?1",
            params![target.id, key],
        )?;
        let now = now_ms();
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "task.moved",
            &format!("#{} moved", target.id),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete(&mut self, target: VersionedTask) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (_, status) = checked(&tx, self.project_id, target)?;
        require(
            matches!(status.as_str(), "ready" | "new"),
            "Only pending work can be deleted",
        )?;
        let now = now_ms();
        tx.execute("UPDATE tasks SET lifecycle = 'deleted', closed_at_ms = max(created_at_ms, ?2), resolution_reason = 'Deleted by command', resolved_by_kind = ?3, resolved_by = ?4 WHERE id = ?1", params![target.id, now, self.actor.kind.as_str(), self.actor.name])?;
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "task.deleted",
            &format!("#{} deleted; definition retained for Undo", target.id),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Restore the retained order key unless doing so would precede running work.
    pub fn restore(&mut self, target: VersionedTask) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (lifecycle, _) = checked(&tx, self.project_id, target)?;
        require(lifecycle == "deleted", "Task is no longer deleted")?;
        let blocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks t JOIN attempts a ON a.task_id = t.id WHERE t.project_id = ?1 AND a.state = 'running' AND t.order_key > (SELECT order_key FROM tasks WHERE id = ?2))", params![self.project_id, target.id], |r| r.get(0))?;
        require(!blocked, "Cannot restore before running current work")?;
        let now = now_ms();
        tx.execute("UPDATE tasks SET lifecycle = CASE WHEN EXISTS(SELECT 1 FROM task_revisions WHERE task_id = ?1 AND lifecycle = 'ready') THEN 'ready' ELSE 'new' END, closed_at_ms = NULL, resolution_reason = NULL, resolved_by_kind = NULL, resolved_by = NULL WHERE id = ?1", [target.id])?;
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "task.restored",
            &format!("#{} restored", target.id),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn resolve(
        &mut self,
        target: VersionedTask,
        resolution: Resolution,
        reason: &str,
    ) -> Result<()> {
        require(!reason.trim().is_empty(), "A reason is required")?;
        self.close_current(target, resolution.as_str(), reason, false)
    }
    pub fn acknowledge(&mut self, target: VersionedTask) -> Result<()> {
        require(
            matches!(self.actor.kind, ActorKind::Human),
            "A Human task requires human acknowledgment",
        )?;
        self.close_current(target, "succeeded", "Human task acknowledged", true)
    }
    fn close_current(
        &mut self,
        target: VersionedTask,
        lifecycle: &str,
        reason: &str,
        human: bool,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        checked(&tx, self.project_id, target)?;
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT status, kind FROM current_tasks WHERE project_id = ?1 AND id = ?2",
                params![self.project_id, target.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let valid = current.is_some_and(|(status, kind)| {
            if human {
                status == "ready" && kind == "human"
            } else {
                matches!(status.as_str(), "failed" | "interrupted" | "cancelled")
            }
        });
        require(
            valid,
            "Only failed current can be resolved, or ready Human current acknowledged",
        )?;
        let now = now_ms();
        tx.execute("UPDATE tasks SET lifecycle = ?2, closed_at_ms = max(created_at_ms, ?3), resolution_reason = ?4, resolved_by_kind = ?5, resolved_by = ?6 WHERE id = ?1", params![target.id, lifecycle, now, reason, self.actor.kind.as_str(), self.actor.name])?;
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "task.resolved",
            &format!("#{} {lifecycle}: {reason}", target.id),
        )?;
        tx.commit()?;
        Ok(())
    }
}

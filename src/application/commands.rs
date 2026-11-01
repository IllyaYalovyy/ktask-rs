use super::lifecycle::{Action, State};
use super::*;
use crate::storage::ordering;

fn checked(tx: &Transaction<'_>, project: i64, target: VersionedTask) -> Result<(String, String)> {
    tx.query_row("SELECT lifecycle, status FROM task_details WHERE project_id = ?1 AND id = ?2 AND row_version = ?3",
        params![project, target.id, target.version], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
        .ok_or_else(|| Error::Conflict("Task changed elsewhere; cancel and reopen this form".into()))
}
/// Tasks are editable whenever they are not running or finished. Resume and Retry then
/// continue the work with the new revision.
fn editable(tx: &Transaction<'_>, project: i64, target: VersionedTask) -> Result<()> {
    let lifecycle: String = tx.query_row(
        "SELECT lifecycle FROM tasks WHERE id=?1 AND project_id=?2",
        params![target.id, project],
        |r| r.get(0),
    )?;
    let state = allowed(tx, project, target, Action::Edit)?;
    require(
        matches!(lifecycle.as_str(), "new" | "ready"),
        &state.refusal(Action::Edit),
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
    tx.execute("INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by, attempt_limit, attempt_timeout_ms, created_at_ms, decided_at_ms, decided_by_kind, decided_by, stage_timeout_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![task, number, if ready { "ready" } else { "draft" }, definition.kind.as_str(), definition.title, definition.goal, actor.kind.as_str(), actor.name, definition.attempt_limit, definition.timeout_ms, now,
            ready.then_some(now), ready.then_some(actor.kind.as_str()), ready.then_some(&actor.name), definition.stage_timeout_ms])?;
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
    /// A worker capability can only insert a draft after its active current task.
    pub fn file_follow_up(
        &mut self,
        token: &str,
        expected_task: i64,
        expected_attempt: i64,
        definition: &Definition,
    ) -> Result<i64> {
        require(!token.trim().is_empty(), "Worker token is required")?;
        definition.validate()?;
        self.validate_definition_models(definition)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source: Option<(i64, i64, String)> = tx.query_row(
            "SELECT a.task_id, a.id, t.order_key FROM attempts a JOIN current_tasks t ON t.id=a.task_id AND t.project_id=a.project_id WHERE a.project_id=?1 AND a.execution_token=?2 AND a.state='running' AND EXISTS (SELECT 1 FROM attempt_steps s WHERE s.attempt_id=a.id AND s.role IN ('execution','remediation') AND s.state='running')",
            params![self.project_id, token],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let (origin_task, origin_attempt, mut lower) = source.ok_or_else(|| {
            Error::Conflict(
                "Worker callback requires an active execution attempt on current task".into(),
            )
        })?;
        require(
            origin_task == expected_task && origin_attempt == expected_attempt,
            "Worker callback source changed",
        )?;
        let mut upper = None;
        {
            let mut following = tx.prepare("SELECT order_key,origin_task_id FROM tasks WHERE project_id=?1 AND order_key>?2 ORDER BY order_key")?;
            for row in following.query_map(params![self.project_id, lower], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })? {
                let (key, parent) = row?;
                if parent == Some(origin_task) {
                    lower = key;
                } else {
                    upper = Some(key);
                    break;
                }
            }
        }
        let key = ordering::between(Some(&lower), upper.as_deref())?;
        let now = now_ms();
        tx.execute("INSERT INTO tasks(project_id,order_key,lifecycle,origin,origin_task_id,origin_attempt_id,created_at_ms,updated_at_ms) VALUES (?1,?2,'new','worker',?3,?4,?5,?5)",
            params![self.project_id,key,origin_task,origin_attempt,now])?;
        let id = tx.last_insert_rowid();
        insert_revision(
            &tx,
            id,
            1,
            definition,
            false,
            now,
            &Actor {
                kind: ActorKind::Ai,
                name: format!("worker attempt #{origin_attempt}"),
            },
        )?;
        event(
            &tx,
            self.project_id,
            id,
            now,
            "task.created",
            &format!(
                "#{id} follow-up draft from task #{origin_task}, attempt #{origin_attempt}: {}",
                definition.title
            ),
        )?;
        tx.commit()?;
        Ok(id)
    }
    /// Automatic judge handoffs share one durable budget, including inserted prerequisites.
    pub fn automatic_replans(&self, task: i64) -> Result<i64> {
        let count = self.db.connection().query_row(
            "SELECT root.automatic_replans FROM tasks current JOIN tasks root ON root.id=coalesce(current.recovery_root_id,current.id) WHERE current.id=?1 AND current.project_id=?2",
            params![task, self.project_id], |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn record_automatic_replan(&mut self, task: i64) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root: i64 = tx.query_row(
            "SELECT coalesce(recovery_root_id,id) FROM tasks WHERE id=?1 AND project_id=?2",
            params![task, self.project_id],
            |row| row.get(0),
        )?;
        require(tx.execute(
            "UPDATE tasks SET automatic_replans=automatic_replans+1 WHERE id=?1 AND project_id=?2 AND automatic_replans<10",
            params![root, self.project_id],
        )? == 1, "Automatic recovery exhausted after 10 judge replans; edit current or apply manual remediation")?;
        tx.commit()?;
        Ok(())
    }
    /// Replace pending work or failed current with planner tasks in one transaction.
    pub fn apply_breakdown(
        &mut self,
        target: VersionedTask,
        subtasks: &[Definition],
        reason: &str,
        ready: bool,
    ) -> Result<Vec<i64>> {
        require(!subtasks.is_empty(), "Breakdown needs subtasks")?;
        require(!reason.trim().is_empty(), "Breakdown needs a reason")?;
        for definition in subtasks {
            definition.validate()?;
            self.validate_definition_models(definition)?;
        }
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Breaking a task down supersedes it, so only Pending work qualifies.
        allowed(&tx, self.project_id, target, Action::Supersede)?;
        let keys = ordering::keys_before(&tx, self.project_id, Some(target.id), subtasks.len())?;
        let now = now_ms();
        let planner = Actor {
            kind: ActorKind::Ai,
            name: "planner".into(),
        };
        let mut ids = Vec::new();
        for (definition, key) in subtasks.iter().zip(keys) {
            tx.execute("INSERT INTO tasks(project_id,order_key,lifecycle,origin,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'planner',?4,?4)", params![self.project_id,key,if ready {"ready"} else {"new"},now])?;
            let id = tx.last_insert_rowid();
            insert_revision(&tx, id, 1, definition, ready, now, &planner)?;
            tx.execute(
                "UPDATE task_revisions SET change_reason=?2 WHERE task_id=?1 AND revision=1",
                params![id, reason],
            )?;
            event(&tx, self.project_id, id, now, "task.planned", reason)?;
            ids.push(id);
        }
        tx.execute("UPDATE tasks SET lifecycle='superseded', closed_at_ms=max(created_at_ms,?2), resolution_reason=?3, resolved_by_kind='ai', resolved_by='planner' WHERE id=?1", params![target.id,now,reason])?;
        lifecycle_event(
            &tx,
            target.id,
            "closed",
            Some("superseded"),
            ("ai", "planner"),
            reason,
            now,
        )?;
        bump(&tx, target, now)?;
        event(
            &tx,
            self.project_id,
            target.id,
            now,
            "task.superseded",
            reason,
        )?;
        tx.commit()?;
        Ok(ids)
    }
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
        let existing = self
            .task(target.id)?
            .ok_or_else(|| Error::Conflict("Task disappeared".into()))?
            .definition()
            .definition
            .clone();
        if let Some(model) = &model {
            let mut revised = existing.clone();
            revised.models[0] = Some(model.clone());
            self.validate_definition_models(&revised)?;
        } else {
            self.validate_definition_models(&existing)?;
        }
        for definition in repairs {
            self.validate_definition_models(definition)?;
        }
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (lifecycle, status) = checked(&tx, self.project_id, target)?;
        // The failure judge applies its plan after ending the attempt it judged.
        require(
            lifecycle == "ready" && matches!(status.as_str(), "ready" | "failed"),
            "Only a stopped current task can be remediated",
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
            let mut definition = existing.clone();
            let used: i64 = tx.query_row(
                "SELECT attempts_used FROM task_details WHERE id=?1",
                [target.id],
                |row| row.get(0),
            )?;
            if definition.attempt_limit.is_some_and(|limit| limit <= used) {
                definition.attempt_limit = Some(used.saturating_add(1));
            }
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
            let used: i64 = tx.query_row(
                "SELECT attempts_used FROM task_details WHERE id=?1",
                [target.id],
                |row| row.get(0),
            )?;
            if existing.attempt_limit.is_some_and(|limit| limit <= used) {
                let mut revised = existing.clone();
                revised.attempt_limit = Some(used.saturating_add(1));
                let number: i64 = tx.query_row(
                    "SELECT max(revision)+1 FROM task_revisions WHERE task_id=?1",
                    [target.id],
                    |row| row.get(0),
                )?;
                insert_revision(&tx, target.id, number, &revised, true, now, &remediator)?;
                tx.execute(
                    "UPDATE task_revisions SET change_reason=?3 WHERE task_id=?1 AND revision=?2",
                    params![
                        target.id,
                        number,
                        "One bounded retry after prerequisite work"
                    ],
                )?;
                bump(&tx, target, now)?;
                event(
                    &tx,
                    self.project_id,
                    target.id,
                    now,
                    "remediation.retry_reserved",
                    "One bounded retry reserved in a new revision; earlier attempts and failures retained",
                )?;
            }
            let keys = ordering::keys_before(&tx, self.project_id, Some(target.id), repairs.len())?;
            for (definition, key) in repairs.iter().zip(keys) {
                tx.execute("INSERT INTO tasks(project_id,order_key,lifecycle,origin,recovery_root_id,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'remediator',(SELECT coalesce(recovery_root_id,id) FROM tasks WHERE id=?5),?4,?4)", params![self.project_id,key,if ready {"ready"} else {"new"},now,target.id])?;
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
        if let Some(first) = ids.first() {
            tx.execute("UPDATE attempt_delivery SET handoff_target=?2 WHERE handoff_target=?1 AND handoff_claimed_by IS NULL AND attempt_id IN (SELECT id FROM attempts WHERE project_id=?3 AND state IN ('failed','interrupted'))",params![target.id,first,self.project_id])?;
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
        for definition in definitions {
            self.validate_definition_models(definition)?;
        }
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
        let actor = self.actor.clone();
        self.revise_as(target, definition, ready, &actor, None)
    }
    /// A revision by an explicit actor, such as the failure judge, with its reason recorded.
    pub fn revise_as(
        &mut self,
        target: VersionedTask,
        definition: &Definition,
        ready: bool,
        actor: &Actor,
        reason: Option<&str>,
    ) -> Result<()> {
        self.validate_definition_models(definition)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        editable(&tx, self.project_id, target)?;
        let (number, old_kind): (i64, String) = tx.query_row(
            "SELECT revision + 1, kind FROM task_revisions WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
            [target.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        require(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1)",
                [target.id],
                |r| r.get::<_, bool>(0),
            )? || old_kind == definition.kind.as_str(),
            "Keep the kind of an attempted task",
        )?;
        let now = now_ms();
        insert_revision(&tx, target.id, number, definition, ready, now, actor)?;
        if let Some(reason) = reason {
            tx.execute(
                "UPDATE task_revisions SET change_reason=?3 WHERE task_id=?1 AND revision=?2",
                params![target.id, number, reason],
            )?;
        }
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
            &reason.map_or_else(
                || format!("#{} revision {number} saved", target.id),
                |reason| {
                    format!(
                        "#{} revision {number} saved by {}: {reason}",
                        target.id, actor.name
                    )
                },
            ),
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
        editable(&tx, self.project_id, target)?;
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
            // A decided revision makes a stopped task Pending: the next Run starts fresh.
            tx.execute(
                "UPDATE tasks SET lifecycle = 'ready', reset_pending = CASE WHEN EXISTS(SELECT 1 FROM attempts WHERE task_id = ?1 AND state IN ('running','paused','waiting')) THEN reset_pending ELSE 1 END WHERE id = ?1",
                [target.id],
            )?;
        } else {
            tx.execute("UPDATE tasks SET lifecycle = 'cancelled', closed_at_ms = max(created_at_ms, ?2), resolution_reason = ?3, resolved_by_kind = ?4, resolved_by = ?5 WHERE id = ?1", params![target.id, now, reason, self.actor.kind.as_str(), self.actor.name])?;
            lifecycle_event(
                &tx,
                target.id,
                "closed",
                Some("cancelled"),
                (self.actor.kind.as_str(), &self.actor.name),
                reason,
                now,
            )?;
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
        allowed(&tx, self.project_id, target, Action::Move)?;
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

    /// Pending only: give up on the task. It moves to History with any earlier attempts.
    pub fn cancel(&mut self, target: VersionedTask, reason: &str) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        allowed(&tx, self.project_id, target, Action::Cancel)?;
        let reason = reason_or(reason, "Cancelled", &self.actor);
        close(
            &tx,
            self.project_id,
            &self.actor,
            target,
            "cancelled",
            &reason,
            now_ms(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Pending only: record that other tasks of this project replace this one.
    pub fn supersede(&mut self, target: VersionedTask, by: &[i64], reason: &str) -> Result<()> {
        require(
            !by.is_empty(),
            "Supersede needs at least one replacing task (--by ID)",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        allowed(&tx, self.project_id, target, Action::Supersede)?;
        for &id in by {
            require(id != target.id, "A task cannot supersede itself")?;
            require(
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND project_id=?2)",
                    params![id, self.project_id],
                    |r| r.get::<_, bool>(0),
                )?,
                &format!("Task #{id} is not in this project"),
            )?;
            tx.execute("INSERT OR IGNORE INTO task_links(project_id,task_id,related_task_id,kind) VALUES (?1,?2,?3,'replaces')", params![self.project_id,id,target.id])?;
        }
        let list = by
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        let reason = if reason.trim().is_empty() {
            format!("Superseded by {list}")
        } else {
            format!("Superseded by {list}: {}", reason.trim())
        };
        close(
            &tx,
            self.project_id,
            &self.actor,
            target,
            "superseded",
            &reason,
            now_ms(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A finished task runs again as a new Pending task with the same definition, linked
    /// to the original. The original and its attempts stay in History unchanged.
    pub fn copy_to_queue(&mut self, target: VersionedTask, before: Option<i64>) -> Result<i64> {
        let task = self
            .task(target.id)?
            .filter(|t| t.version == target.version)
            .ok_or_else(|| Error::Conflict("Task changed elsewhere; refresh the task".into()))?;
        task.require(Action::CopyToQueue)?;
        let definition = task.definition().definition.clone();
        let ready = matches!(self.actor.kind, ActorKind::Human);
        let id = self.create(&[definition], before, ready)?.remove(0);
        self.db.connection().execute("INSERT INTO task_links(project_id,task_id,related_task_id,kind) VALUES (?1,?2,?3,'derived_from')", params![self.project_id,id,target.id])?;
        Ok(id)
    }
}

/// Refuse `action` unless this exact task version's state offers it.
pub(super) fn allowed(
    tx: &Transaction<'_>,
    project: i64,
    target: VersionedTask,
    action: Action,
) -> Result<State> {
    let (status, wait): (String, Option<String>) = tx
        .query_row(
            "SELECT status, wait_reason FROM task_details WHERE project_id=?1 AND id=?2 AND row_version=?3",
            params![project, target.id, target.version],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| Error::Conflict("Task changed elsewhere; refresh the task".into()))?;
    let state = State::of(&status, wait.as_deref());
    require(state.allows(action), &state.refusal(action))?;
    Ok(state)
}

/// The owner's reason, or who did it when none was given.
pub(super) fn reason_or(reason: &str, verb: &str, actor: &Actor) -> String {
    if reason.trim().is_empty() {
        format!("{verb} by {}", actor.name)
    } else {
        reason.trim().to_owned()
    }
}

/// What happened, with the owner's reason: "Stopped: too slow", or "Stopped by owner".
pub(super) fn labelled(reason: &str, verb: &str, actor: &Actor) -> String {
    if reason.trim().is_empty() {
        format!("{verb} by {}", actor.name)
    } else {
        format!("{verb}: {}", reason.trim())
    }
}

/// Move a task to History with its final lifecycle; attempts and evidence are untouched.
pub(super) fn close(
    tx: &Transaction<'_>,
    project: i64,
    actor: &Actor,
    target: VersionedTask,
    lifecycle: &str,
    reason: &str,
    now: i64,
) -> Result<()> {
    tx.execute("UPDATE tasks SET lifecycle = ?2, closed_at_ms = max(created_at_ms, ?3), resolution_reason = ?4, resolved_by_kind = ?5, resolved_by = ?6 WHERE id = ?1", params![target.id, lifecycle, now, reason, actor.kind.as_str(), actor.name])?;
    lifecycle_event(
        tx,
        target.id,
        "closed",
        Some(lifecycle),
        (actor.kind.as_str(), &actor.name),
        reason,
        now,
    )?;
    bump(tx, target, now)?;
    event(
        tx,
        project,
        target.id,
        now,
        "task.resolved",
        &format!("#{} {lifecycle}: {reason}", target.id),
    )
}

pub(super) fn lifecycle_event(
    tx: &Transaction<'_>,
    task: i64,
    action: &str,
    outcome: Option<&str>,
    actor: (&str, &str),
    reason: &str,
    now: i64,
) -> Result<()> {
    tx.execute("INSERT INTO task_lifecycle_events(task_id,action,outcome,actor_kind,actor,reason,occurred_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![task,action,outcome,actor.0,actor.1,reason,now])?;
    Ok(())
}

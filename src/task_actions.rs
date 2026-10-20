use crate::{
    app::{App, View},
    dialog::{Dialog, DialogKind, Field},
    presentation::{ROLES, RevisionState, Status, Task},
};

impl App {
    pub(crate) fn open_models(&mut self) {
        let Some(task) = self.selected_task() else {
            return;
        };
        let revision = task.draft().unwrap_or_else(|| task.definition());
        let mut fields = Vec::new();
        for (role, model) in ROLES.iter().zip(&revision.settings.models) {
            fields.push(Field::text(&format!("{role} model"), &model.name));
            fields.push(Field::choice(
                &format!("{role} effort"),
                &["none", "low", "medium", "high", "max"],
                model.effort.as_deref().unwrap_or("none"),
            ));
        }
        fields.push(Field::text(
            "Attempt limit (blank = unlimited)",
            &if revision.settings.attempt_limit == 0 {
                String::new()
            } else {
                revision.settings.attempt_limit.to_string()
            },
        ));
        fields.push(Field::text(
            "Active time limit ms (task-wide; blank = unlimited)",
            &if revision.settings.timeout_ms == 0 {
                String::new()
            } else {
                revision.settings.timeout_ms.to_string()
            },
        ));
        self.dialog = Some(Dialog::new(
            DialogKind::Models(task.id),
            "Task models and limits",
            "Save creates a Human-approved revision. Earlier attempts keep their inputs.\nTab or Fields +/- reaches every setting; none means no effort setting.",
            fields,
        ));
    }

    pub(crate) fn save_models(&mut self, dialog: &Dialog, id: usize) -> Result<(), String> {
        if (0..4).any(|n| dialog.value(n * 2).is_empty()) {
            return Err("Every role needs a model".into());
        }
        let attempt_limit = limit(dialog.value(8))?;
        let timeout_ms = limit(dialog.value(9))?;
        let task = self
            .queue
            .iter_mut()
            .find(|t| {
                t.id == id && matches!(t.status(), Status::New | Status::Pending | Status::Failed)
            })
            .ok_or("Task is read-only")?;
        let mut revision = task.draft().unwrap_or_else(|| task.definition()).clone();
        for (n, model) in revision.settings.models.iter_mut().enumerate() {
            model.name = dialog.value(n * 2).into();
            model.effort =
                (dialog.value(n * 2 + 1) != "none").then(|| dialog.value(n * 2 + 1).to_owned());
        }
        revision.settings.attempt_limit =
            usize::try_from(attempt_limit).map_err(|_| "Attempt limit is too large")?;
        revision.settings.timeout_ms = timeout_ms;
        revision.state = RevisionState::Ready;
        revision.actor = "Human: owner".into();
        task.revise(revision);
        self.record(format!(
            "#{id} models saved · new approved revision · previous attempts unchanged"
        ));
        Ok(())
    }

    pub(crate) fn approve_selected(&mut self) {
        let Some(id) = self.selected_task().map(|t| t.id) else {
            return;
        };
        let task = self
            .queue
            .iter_mut()
            .find(|t| t.id == id)
            .expect("queue task");
        let number = task.draft().expect("enabled draft").number;
        let revision = task
            .revisions
            .iter_mut()
            .find(|r| r.number == number)
            .expect("draft");
        revision.state = RevisionState::Ready;
        revision.actor = "Human: owner".into();
        self.record(format!(
            "#{id} revision {number} approved by Human · ready for execution"
        ));
        if self.auto_advance
            && self
                .current()
                .is_some_and(|t| t.id == id && t.definition().kind == crate::presentation::Kind::Ai)
        {
            self.start_current();
        }
    }

    pub(crate) fn open_reject(&mut self) {
        let task = self.selected_task().expect("enabled draft");
        self.dialog = Some(Dialog::new(
            DialogKind::Reject(task.id),
            "Reject pending revision",
            "Previous approved inputs remain usable. With no approved revision, the task moves to History.",
            vec![Field::text("Reason", "")],
        ));
    }

    pub(crate) fn reject_revision(&mut self, dialog: &Dialog, id: usize) -> Result<(), String> {
        if dialog.value(0).is_empty() {
            return Err("A reason is required".into());
        }
        let index = self
            .queue
            .iter()
            .position(|t| t.id == id)
            .ok_or("Task no longer queued")?;
        let task = &mut self.queue[index];
        let number = task.draft().ok_or("No pending revision")?.number;
        let revision = task
            .revisions
            .iter_mut()
            .find(|r| r.number == number)
            .expect("draft");
        revision.state = RevisionState::Rejected;
        revision.actor = format!("Human rejected: {}", dialog.value(0));
        if task.ready().is_none() {
            let mut task = self.queue.remove(index);
            task.resolution = Some(Status::Rejected);
            task.resolution_reason = format!("Human: {}", dialog.value(0));
            self.history.insert(0, task);
        }
        self.record(format!(
            "#{id} revision {number} rejected · approved inputs and attempts retained"
        ));
        Ok(())
    }

    pub(crate) fn open_resolution(&mut self) {
        let task = self.current().expect("failed current");
        self.dialog = Some(Dialog::new(
            DialogKind::Resolve(task.id),
            "Resolve failed current",
            "Moves this task to History. Its failed attempts and evidence are unchanged.",
            vec![
                Field::choice(
                    "Outcome",
                    &["Succeeded override", "Failed skipped", "Superseded"],
                    "Succeeded override",
                ),
                Field::text("Reason", ""),
            ],
        ));
    }

    pub(crate) fn resolve_failed(&mut self, dialog: &Dialog, id: usize) -> Result<(), String> {
        if dialog.value(1).is_empty() {
            return Err("A reason is required".into());
        }
        if !self
            .current()
            .is_some_and(|t| t.id == id && t.status() == Status::Failed)
        {
            return Err("Only failed current can be resolved".into());
        }
        let mut task = self.queue.remove(0);
        task.resolution = Some(match dialog.value(0) {
            "Failed skipped" => Status::Skipped,
            "Superseded" => Status::Superseded,
            _ => Status::Override,
        });
        task.resolution_reason = format!("Human: owner · {}", dialog.value(1));
        self.history.insert(0, task);
        self.history_catalog.selected = Some(id);
        self.follow = true;
        self.log_offset = 0;
        self.record(format!(
            "#{id} resolved: {} · failed attempts retained",
            dialog.value(0)
        ));
        Ok(())
    }

    pub(crate) fn remediate(&mut self, id: usize, insert: bool) -> Result<(), String> {
        if !self
            .current()
            .is_some_and(|t| t.id == id && t.status() == Status::Failed)
        {
            return Err("Only failed current can be remediated".into());
        }
        if insert {
            let mut repair =
                Task::pending(self.next_id, &format!("Repair failing check for #{id}"));
            self.next_id += 1;
            repair.repairs = Some(id);
            repair.revisions[0].actor = "AI: remediator".into();
            repair.revisions[0].goal = self.queue[0].definition().goal.clone();
            repair.revisions[0].check = self.queue[0].definition().check.clone();
            if self.review_required {
                repair.revisions[0].state = RevisionState::Draft;
            }
            self.queue.insert(0, repair);
        } else {
            let task = &mut self.queue[0];
            let mut revision = task.definition().clone();
            revision.settings.models[0] = revision.settings.models[3].clone();
            revision.repaired = true;
            revision.actor = "AI: remediator".into();
            revision.state = if self.review_required {
                RevisionState::Draft
            } else {
                RevisionState::Ready
            };
            task.revise(revision);
        }
        self.switch(View::Current);
        if self.review_required {
            self.record(
                "Remediation needs approval · v Approve or n Reject · previous inputs unchanged",
            );
        } else {
            self.start_current();
            // Do not conceal a blocked handoff or exhausted budget.
            if self
                .current()
                .is_some_and(|t| t.status() == Status::Running)
            {
                self.record("Remediation applied by AI · ordinary queue execution continues");
            }
        }
        Ok(())
    }
}

fn limit(value: &str) -> Result<u64, String> {
    if value.is_empty() {
        return Ok(0);
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "Limits must be positive integers or blank".into())
}

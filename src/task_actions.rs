use crate::{
    app::{App, View},
    application::limits,
    dialog::{Dialog, DialogKind, Field},
    presentation::{ROLES, RevisionState, Status, Task, TaskAction, badge},
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
        for (label, current, standard) in [
            (
                "Task time limit (all attempts)",
                revision.settings.timeout_ms,
                limits::TASK_LIMIT_MS,
            ),
            (
                "Stage time limit (one agent turn)",
                revision.settings.stage_timeout_ms,
                limits::STAGE_LIMIT_MS,
            ),
        ] {
            let current = (current > 0).then_some(current as i64);
            let choices = limits::choices(current, standard);
            fields.push(Field::choice(
                label,
                &choices.iter().map(String::as_str).collect::<Vec<_>>(),
                &limits::label(current),
            ));
        }
        if self.service.is_some() {
            fields.push(Field::choice(
                "Project defaults",
                &["Keep task settings", "Apply current project defaults"],
                "Keep task settings",
            ));
        }
        self.dialog = Some(Dialog::new(
            DialogKind::Models(task.id),
            "Task models and limits",
            "Save creates a Human-approved revision. Earlier attempts keep their inputs.\nTab or Fields +/- reaches every setting; none means no effort setting.\nTime limits are off unless chosen; the stage limit restarts with every agent result.",
            fields,
        ));
    }

    pub(crate) fn save_models(&mut self, dialog: &Dialog, id: usize) -> Result<(), String> {
        if (0..4).any(|n| dialog.value(n * 2).is_empty()) {
            return Err("Every role needs a model".into());
        }
        let attempt_limit = limit(dialog.value(8))?;
        let ms = |index| limits::parse(dialog.value(index)).map(|ms| ms.unwrap_or(0) as u64);
        let (timeout_ms, stage_timeout_ms) = (ms(9)?, ms(10)?);
        let task = self
            .queue
            .iter_mut()
            .find(|t| t.id == id && t.status().allows(TaskAction::Edit))
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
        revision.settings.stage_timeout_ms = stage_timeout_ms;
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
            task.resolution = Some(Status::Cancelled);
            task.resolution_reason = format!("Human: {}", dialog.value(0));
            self.history.insert(0, task);
        }
        self.record(format!(
            "#{id} revision {number} rejected · approved inputs and attempts retained"
        ));
        Ok(())
    }

    /// The dialog for a task control: Enter applies it, Esc changes nothing.
    pub(crate) fn open_control(&mut self, action: crate::app::Action) {
        use crate::app::Action;
        let target = if action == Action::Stop {
            self.stop_target()
        } else {
            self.selected_task()
        };
        let Some(task) = target else {
            return;
        };
        let name = format!("#{} {}", task.id, task.definition().title);
        let reason = || Field::text("Reason (optional)", "");
        let (kind, title, body, fields) = match action {
            Action::Stop => (
                DialogKind::Stop(task.id),
                format!("Stop {name}"),
                "The worker stops and this attempt ends. Its uncommitted changes are saved as a git stash (git stash pop restores them).\nThe task returns to Pending; Run starts it fresh with full limits.",
                vec![reason()],
            ),
            Action::MarkDone => (
                DialogKind::MarkDone(task.id),
                format!("Mark done {name}"),
                "Records the task as done by your decision and moves it to History.\nA live attempt stops first and its uncommitted changes are stashed; commit them first to keep them in the tree.",
                vec![reason()],
            ),
            Action::CancelTask => (
                DialogKind::CancelTask(task.id),
                format!("Cancel {name}"),
                "The task moves to History as cancelled. Copy to queue brings it back as a new task.",
                vec![reason()],
            ),
            Action::Supersede => (
                DialogKind::Supersede(task.id),
                format!("Supersede {name}"),
                "Name the tasks that replace this one. It moves to History as superseded.",
                vec![
                    Field::text("Replaced by task IDs (comma separated)", ""),
                    reason(),
                ],
            ),
            Action::CopyToQueue => (
                DialogKind::CopyToQueue(task.id),
                format!("Copy to queue {name}"),
                "Adds a new pending task with the same definition. The original stays in History unchanged.",
                vec![Field::text("Before task ID (blank = end of Queue)", "")],
            ),
            _ => return,
        };
        self.dialog = Some(Dialog::new(kind, &title, body, fields));
    }

    /// Stop applies to the current task from every screen, as Pause does.
    pub(crate) fn stop_target(&self) -> Option<&Task> {
        if self.view == View::Current {
            self.current()
        } else {
            self.selected_task()
                .filter(|t| t.status().allows(TaskAction::Stop))
        }
    }

    /// Task IDs typed by the owner: digits separated by commas or spaces, at least one.
    pub(crate) fn task_ids(text: &str) -> Result<Vec<usize>, String> {
        let ids = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.trim_start_matches('#')
                    .parse::<usize>()
                    .map_err(|_| format!("{part} is not a task ID"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if ids.is_empty() {
            return Err("Name at least one replacing task".into());
        }
        Ok(ids)
    }

    /// Demo mode: apply a task control dialog to the in-memory queue.
    pub(crate) fn apply_demo_control(&mut self, dialog: &Dialog) -> Result<(), String> {
        // Same wording as the real service: "Stopped by owner" or "Stopped: too slow".
        let note = |reason: &str, verb: &str| {
            if reason.trim().is_empty() {
                format!("{verb} by owner")
            } else {
                format!("{verb}: {}", reason.trim())
            }
        };
        match dialog.kind.clone() {
            DialogKind::Stop(id) => {
                let task = self
                    .queue
                    .iter_mut()
                    .find(|t| t.id == id && t.status().allows(TaskAction::Stop))
                    .ok_or("The task can no longer be stopped")?;
                match task.attempts.last_mut() {
                    // A failed attempt stays as recorded; only the task returns to Pending.
                    Some(attempt) if attempt.state == Status::Failed => {
                        task.stored_status = Some(Status::Pending)
                    }
                    Some(attempt) => {
                        attempt.state = Status::Pending;
                        attempt.resume_at = None;
                        attempt.outcome = note(dialog.value(0), "Stopped");
                        attempt.logs.push(attempt.outcome.clone());
                    }
                    None => {}
                }
                self.record(format!(
                    "#{id} stopped · changes stashed (mock) · Run starts it fresh"
                ));
            }
            DialogKind::MarkDone(id) => {
                self.close_demo(id, Status::Done, &note(dialog.value(0), "Marked done"))?
            }
            DialogKind::CancelTask(id) => {
                self.close_demo(id, Status::Cancelled, &note(dialog.value(0), "Cancelled"))?
            }
            DialogKind::Supersede(id) => {
                let by = Self::task_ids(dialog.value(0))?;
                for other in &by {
                    if *other == id || self.task(*other).is_none() {
                        return Err(format!("#{other} cannot replace #{id}"));
                    }
                }
                let list = by
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.close_demo(id, Status::Superseded, &format!("Superseded by {list}"))?
            }
            DialogKind::CopyToQueue(id) => {
                let source = self
                    .history
                    .iter()
                    .find(|t| t.id == id)
                    .ok_or("Only finished tasks can be copied")?;
                let mut copy = Task::pending(self.next_id, &source.definition().title);
                copy.revisions[0] = source.definition().clone();
                copy.revisions[0].number = 1;
                copy.revisions[0].state = RevisionState::Ready;
                copy.revisions[0].actor = "Human: owner".into();
                copy.id = self.next_id;
                self.next_id += 1;
                let position = self.insertion_position(dialog.value(0), None)?;
                let new = copy.id;
                self.queue.insert(position, copy);
                self.record(format!("#{id} copied to the queue as #{new}"));
            }
            _ => {}
        }
        Ok(())
    }

    /// Demo mode: move a task to History with `status`.
    fn close_demo(&mut self, id: usize, status: Status, reason: &str) -> Result<(), String> {
        let action = match status {
            Status::Done => TaskAction::MarkDone,
            Status::Superseded => TaskAction::Supersede,
            _ => TaskAction::Cancel,
        };
        let index = self
            .queue
            .iter()
            .position(|t| t.id == id && t.status().allows(action))
            .ok_or_else(|| format!("{} is no longer available for #{id}", action.label()))?;
        let mut task = self.queue.remove(index);
        if let Some(attempt) = task.attempts.last_mut().filter(|a| {
            matches!(
                a.state,
                Status::Running | Status::Paused | Status::NeedsInput
            )
        }) {
            attempt.state = Status::Pending;
            attempt.outcome = format!("Ended: {reason}");
        }
        task.resolution = Some(status);
        task.resolution_reason = format!("Human: owner · {reason}");
        self.history.insert(0, task);
        self.history_catalog.selected = Some(id);
        self.follow = true;
        self.log_offset = 0;
        self.record(format!("#{id} {} · {reason}", badge(status).to_lowercase()));
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

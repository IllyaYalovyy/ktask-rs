//! The `--demo` workspace: fixtures and a simulated runner in memory. It never opens SQLite.
use crate::{
    app::{Action, Catalog, EvidenceSection, Screen, View},
    backend::{Backend, Column, EvidenceAt},
    dialog::{Dialog, DialogKind, Field},
    forms, mock,
    presentation::{
        Attempt, Kind, ROLES, RevisionState, Status, Task, TaskAction, badge, budget, duration,
        model_label, money,
    },
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const STEP_DELAY: &str = "Step delay (ms)";
const DATASET: &str = "Demo dataset";
const CLEANUP: &str = "Cleanup simulation";
const RETRY: &str = "Same-task retry";
const CONTINUE: &str = "Continue queue";

pub(crate) struct DemoWorkspace {
    files: BTreeMap<String, String>,
    next_id: usize,
    tick_ms: u64,
    review_required: bool,
    cleanup_fail_once: bool,
    clean_retry: bool,
    last_workspace: Option<usize>,
}

impl DemoWorkspace {
    /// The workspace with fixture `dataset` on `screen`.
    pub fn load(screen: &mut Screen, dataset: &str) -> Self {
        (screen.queue, screen.history) = mock::dataset(dataset);
        screen.queue_catalog.selected = screen.queue.first().map(|t| t.id);
        screen.history_catalog.selected = screen.history.first().map(|t| t.id);
        if !screen.queue.is_empty() {
            screen.events = vec![
                "Check failed · #1".into(),
                "Execution stopped".into(),
                "Stopped for a decision".into(),
            ];
            screen.task_events = screen.events.clone();
        }
        Self {
            files: mock::files(),
            next_id: 241,
            tick_ms: 400,
            review_required: false,
            cleanup_fail_once: false,
            clean_retry: false,
            last_workspace: Some(1),
        }
    }

    fn start_current(&mut self, screen: &mut Screen) {
        let Some(task) = screen.current() else { return };
        if task.ready().is_none() {
            screen.message("Approve a revision before execution");
            return;
        }
        let resume = task.status() == Status::Paused;
        let id = task.id;
        let cleanup = self.last_workspace != Some(id) || (!resume && self.clean_retry);
        if cleanup {
            screen.record(format!(
                "Workspace #{id}: useful work checkpointed (mock); existing attempts retained"
            ));
            if self.cleanup_fail_once {
                self.cleanup_fail_once = false;
                screen.record(
                    "Cleanup blocked (mock) · work preserved · retry Run/Retry to check again",
                );
                return;
            }
            screen.record(format!("Workspace #{id}: owned scratch cleaned (mock)"));
        } else {
            screen.record(format!(
                "Workspace #{id}: retained for same-task retry/resume (mock)"
            ));
        }
        self.last_workspace = Some(id);
        let task = screen.queue.first_mut().expect("current");
        if resume {
            if let Some(attempt) = task.attempts.last_mut() {
                attempt.state = Status::Running;
                attempt.outcome = "Running".into();
                attempt.logs.push("Resumed same attempt and session".into());
            }
        } else {
            task.stored_status = None;
            let session = task.attempts.last().and_then(|a| a.session).unwrap_or(id);
            task.attempts
                .push(Attempt::start(task.definition().clone(), session));
        }
        screen.last_tick = Instant::now();
        screen.follow = true;
        screen.record("Current running · simulation only");
    }

    fn complete_current(&mut self, screen: &mut Screen, result: &str) {
        let mut task = screen.queue.remove(0);
        task.resolution = Some(Status::Done);
        if task.attempts.is_empty() {
            task.attempts
                .push(Attempt::start(task.definition().clone(), task.id));
        }
        if let Some(attempt) = task.attempts.last_mut() {
            attempt.state = Status::Done;
            attempt.outcome = result.into();
            attempt.logs.push(result.into());
        }
        let repaired = task.repairs;
        if let Some(target) = repaired.and_then(|id| screen.queue.iter_mut().find(|t| t.id == id)) {
            let mut revision = target.definition().clone();
            revision.repaired = true;
            revision.actor = "AI: remediator".into();
            target.revise(revision);
        }
        screen.record(format!(
            "#{} completed · {}",
            task.id,
            if screen.queue.is_empty() {
                "queue complete"
            } else {
                "next task ready"
            }
        ));
        screen.history_catalog.selected = Some(task.id);
        screen.history.insert(0, task);
        screen.follow = true;
        screen.show_log = false;
        screen.log_offset = 0;
        screen.reconcile();
        if (screen.auto_advance || repaired.is_some())
            && screen.current().is_some_and(|t| {
                t.definition().kind.runs_ai()
                    && (t.status() == Status::Pending
                        || (repaired.is_some() && t.status() == Status::Failed))
            })
        {
            self.start_current(screen);
        }
    }

    fn answer_form(screen: &Screen) -> Option<Dialog> {
        let id = screen.current()?.id;
        Some(forms::answer(
            id,
            "Which export format should be used?\nThe answer is saved and the task continues.",
        ))
    }

    fn approve_selected(&mut self, screen: &mut Screen) {
        let Some(id) = screen.selected_task().map(|t| t.id) else {
            return;
        };
        let task = screen
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
        screen.record(format!(
            "#{id} revision {number} approved by Human · ready for execution"
        ));
        if screen.auto_advance
            && screen
                .current()
                .is_some_and(|t| t.id == id && t.definition().kind.runs_ai())
        {
            self.start_current(screen);
        }
    }

    fn models_form(task: &Task) -> Dialog {
        let revision = task.draft().unwrap_or_else(|| task.definition());
        let mut fields = Vec::new();
        for (role, model) in ROLES.iter().zip(&revision.settings.models) {
            fields.push(Field::text(&format!("{role} model"), &model.name));
            fields.push(Field::choice(
                &format!("{role} effort"),
                &["none", "low", "medium", "high", "xhigh", "max"],
                model.effort.as_deref().unwrap_or("none"),
            ));
        }
        fields.extend(forms::limit_fields(&revision.settings));
        Dialog::new(
            DialogKind::Models(task.id),
            "Task models and limits",
            "Save creates a Human-approved revision. Earlier attempts keep their inputs.\nTab or Fields +/- reaches every setting; none means no effort setting.\nTime limits are off unless chosen; the stage limit restarts with every agent result.",
            fields,
        )
    }

    fn save_models(screen: &mut Screen, dialog: &Dialog, id: usize) -> Result<(), String> {
        if ROLES
            .iter()
            .any(|role| dialog.get(&format!("{role} model")).is_empty())
        {
            return Err("Every role needs a model".into());
        }
        let attempt_limit = limit(dialog.get(forms::ATTEMPT_LIMIT))?;
        let ms = |label| {
            crate::application::limits::parse(dialog.get(label)).map(|ms| ms.unwrap_or(0) as u64)
        };
        let (timeout_ms, stage_timeout_ms) = (ms(forms::TASK_TIME)?, ms(forms::STAGE_TIME)?);
        let task = screen
            .queue
            .iter_mut()
            .find(|t| t.id == id && t.status().allows(TaskAction::Edit))
            .ok_or("Task is read-only")?;
        let mut revision = task.draft().unwrap_or_else(|| task.definition()).clone();
        for (role, model) in ROLES.iter().zip(revision.settings.models.iter_mut()) {
            let effort = dialog.get(&format!("{role} effort"));
            model.name = dialog.get(&format!("{role} model")).into();
            model.effort = (effort != "none").then(|| effort.to_owned());
        }
        revision.settings.attempt_limit =
            usize::try_from(attempt_limit).map_err(|_| "Attempt limit is too large")?;
        revision.settings.timeout_ms = timeout_ms;
        revision.settings.stage_timeout_ms = stage_timeout_ms;
        revision.state = RevisionState::Ready;
        revision.actor = "Human: owner".into();
        task.revise(revision);
        screen.record(format!(
            "#{id} models saved · new approved revision · previous attempts unchanged"
        ));
        Ok(())
    }

    fn reject_revision(screen: &mut Screen, dialog: &Dialog, id: usize) -> Result<(), String> {
        let reason = dialog.get(forms::REJECT_REASON);
        if reason.is_empty() {
            return Err("A reason is required".into());
        }
        let index = screen
            .queue
            .iter()
            .position(|t| t.id == id)
            .ok_or("Task no longer queued")?;
        let task = &mut screen.queue[index];
        let number = task.draft().ok_or("No pending revision")?.number;
        let revision = task
            .revisions
            .iter_mut()
            .find(|r| r.number == number)
            .expect("draft");
        revision.state = RevisionState::Rejected;
        revision.actor = format!("Human rejected: {reason}");
        if task.ready().is_none() {
            let mut task = screen.queue.remove(index);
            task.resolution = Some(Status::Cancelled);
            task.resolution_reason = format!("Human: {reason}");
            screen.history.insert(0, task);
        }
        screen.record(format!(
            "#{id} revision {number} rejected · approved inputs and attempts retained"
        ));
        Ok(())
    }

    /// Apply a task control form to the in-memory queue.
    fn apply_control(&mut self, screen: &mut Screen, dialog: &Dialog) -> Result<(), String> {
        // Same wording as the real service: "Stopped by owner" or "Stopped: too slow".
        let note = |reason: &str, verb: &str| {
            if reason.trim().is_empty() {
                format!("{verb} by owner")
            } else {
                format!("{verb}: {}", reason.trim())
            }
        };
        match dialog.kind {
            DialogKind::Stop(id) | DialogKind::Restart(id) => {
                let task = screen
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
                        attempt.outcome = note(dialog.get(forms::REASON), "Stopped");
                        attempt.logs.push(attempt.outcome.clone());
                    }
                    None => {}
                }
                screen.record(format!(
                    "#{id} stopped · changes stashed (mock) · Run starts it fresh"
                ));
            }
            DialogKind::MarkDone(id) => close(
                screen,
                id,
                Status::Done,
                &note(dialog.get(forms::REASON), "Marked done"),
            )?,
            DialogKind::CancelTask(id) => close(
                screen,
                id,
                Status::Cancelled,
                &note(dialog.get(forms::REASON), "Cancelled"),
            )?,
            DialogKind::Supersede(id) => {
                let by = forms::task_ids(dialog.get(forms::REPLACED_BY))?;
                for other in &by {
                    if *other == id || screen.task(*other).is_none() {
                        return Err(format!("#{other} cannot replace #{id}"));
                    }
                }
                let list = by
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                close(
                    screen,
                    id,
                    Status::Superseded,
                    &format!("Superseded by {list}"),
                )?
            }
            DialogKind::CopyToQueue(id) => {
                let source = screen
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
                let position = screen.insertion_position(dialog.get(forms::COPY_BEFORE), None)?;
                let new = copy.id;
                screen.queue.insert(position, copy);
                screen.record(format!("#{id} copied to the queue as #{new}"));
            }
            _ => {}
        }
        Ok(())
    }

    fn save_task(
        &mut self,
        screen: &mut Screen,
        dialog: &Dialog,
        id: Option<usize>,
        insert: bool,
    ) -> Result<(), String> {
        let (title, goal, check) = (
            dialog.get(forms::TITLE),
            dialog.get(forms::GOAL),
            dialog.get(forms::CHECK),
        );
        if title.is_empty() || goal.is_empty() || check.is_empty() {
            return Err("Title, goal, and acceptance check are required".into());
        }
        let kind = Kind::from_label(dialog.get(forms::KIND)).unwrap_or(Kind::Ai);
        let id = if let Some(id) = id {
            let task = screen
                .queue
                .iter_mut()
                .find(|t| t.id == id && t.status().allows(TaskAction::Edit))
                .ok_or("This task is read-only")?;
            if task.status() == Status::Failed && kind != task.definition().kind {
                return Err("Keep the kind of a failed task; use breakdown to replace it".into());
            }
            let mut revision = task.definition().clone();
            revision.title = title.into();
            revision.goal = goal.into();
            revision.check = check.into();
            revision.kind = kind;
            revision.actor = "Human: owner".into();
            revision.state = if task.ready().is_some() {
                RevisionState::Ready
            } else {
                RevisionState::Draft
            };
            task.revise(revision);
            id
        } else {
            let position = if insert {
                screen.insertion_position(dialog.get(forms::BEFORE), None)?
            } else {
                screen.queue.len()
            };
            let id = self.next_id;
            self.next_id += 1;
            let mut task = Task::pending(id, title);
            let revision = &mut task.revisions[0];
            revision.goal = goal.into();
            revision.check = check.into();
            revision.kind = kind;
            let (model, effort) = (dialog.get(forms::MODEL), dialog.get(forms::EFFORT));
            let execution = &mut revision.settings.models[0];
            if !model.is_empty() {
                execution.name = model.into();
                execution.effort = (!effort.is_empty()).then(|| effort.into());
            } else if !effort.is_empty() {
                execution.effort = Some(effort.into());
            }
            screen.queue.insert(position, task);
            id
        };
        if screen.view != View::Current
            || !matches!(dialog.kind, DialogKind::Task { id: Some(_), .. })
        {
            screen.reveal(id);
        }
        screen.record(format!("#{id} saved · mock workspace"));
        Ok(())
    }

    fn accept_breakdown(
        &mut self,
        screen: &mut Screen,
        dialog: &Dialog,
        id: usize,
    ) -> Result<(), String> {
        let titles = [dialog.get(STEP_ONE), dialog.get(STEP_TWO)];
        if titles.iter().any(|title| title.is_empty()) {
            return Err("Both subtask titles are required".into());
        }
        let index = screen
            .queue
            .iter()
            .position(|t| t.id == id && matches!(t.status(), Status::Pending | Status::Failed))
            .ok_or("Only pending or failed current work can be broken down")?;
        let mut original = screen.queue.remove(index);
        original.resolution = Some(Status::Superseded);
        original.resolution_reason = "Replaced by planner subtasks".into();
        let tasks: Vec<_> = (0..2)
            .map(|n| {
                let mut task = Task::pending(self.next_id + n, titles[n]);
                task.revisions[0].goal = original.definition().goal.clone();
                task.revisions[0].check = original.definition().check.clone();
                task.revisions[0].actor = "AI: planner".into();
                if self.review_required {
                    task.revisions[0].state = RevisionState::Draft;
                }
                task
            })
            .collect();
        let first = self.next_id;
        self.next_id += 2;
        screen.history.insert(0, original);
        screen.queue.splice(index..index, tasks);
        screen.reveal(first);
        screen.record(format!(
            "#{id} replaced by 2 subtasks · original retained in History"
        ));
        Ok(())
    }

    fn answer(screen: &mut Screen, dialog: &Dialog, id: usize) -> Result<(), String> {
        let answer = dialog.get(forms::ANSWER);
        if answer.is_empty() {
            return Err("An answer is required".into());
        }
        let task = screen
            .queue
            .first_mut()
            .filter(|t| t.id == id && t.status() == Status::NeedsInput)
            .ok_or("Current is not waiting for an answer")?;
        task.answer = answer.into();
        if let Some(attempt) = task.attempts.last_mut() {
            attempt.state = Status::Running;
            attempt.logs.push(format!("User answer: {}", task.answer));
            attempt.outcome = "Running".into();
        }
        screen.last_tick = Instant::now();
        screen.record("Answer recorded · current continues");
        Ok(())
    }

    /// Export or overwrite a virtual file; Ok(false) asks to confirm an overwrite first.
    fn export(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        let path = dialog.get(forms::DESTINATION).to_owned();
        if !path.starts_with("/demo/") || !path.ends_with(".tsv") {
            return Err("Use a virtual path /demo/<name>.tsv; no real files are written".into());
        }
        let tasks: Vec<_> = match dialog.get(forms::SCOPE) {
            "History" => screen.history.iter().collect(),
            "All" => screen.queue.iter().chain(&screen.history).collect(),
            _ => screen.queue.iter().collect(),
        };
        let data = tasks
            .iter()
            .map(|t| export_task(t))
            .collect::<Vec<_>>()
            .join("\n");
        if self.files.contains_key(&path) {
            dialog.kind = DialogKind::Overwrite {
                path: path.clone(),
                data,
            };
            dialog.title = "Overwrite mock file?".into();
            dialog.body = format!(
                "{path} already exists.\nConfirm replaces it; cancel preserves its contents."
            );
            dialog.fields.clear();
            dialog.error.clear();
            return Ok(false);
        }
        self.files.insert(path.clone(), data);
        screen.record(format!("Exported to {path} · in memory, NOT disk"));
        Ok(true)
    }

    /// Apply the session settings; a dataset change asks to confirm the reset first.
    fn configure(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        let delay = dialog
            .get(STEP_DELAY)
            .parse::<u64>()
            .ok()
            .filter(|n| (100..=2000).contains(n))
            .ok_or("Step delay must be 100–2000 ms")?;
        let dataset = dialog.get(DATASET).to_owned();
        if dataset != "Keep" && matches!(dialog.kind, DialogKind::Config) {
            dialog.kind = DialogKind::ResetConfig;
            dialog.title = "Reset mock workspace?".into();
            dialog.body = "Apply resets all session tasks to the chosen demo dataset.\nNo production data is touched. Cancel keeps the workspace.".into();
            dialog.error.clear();
            return Ok(false);
        }
        screen.mouse_enabled = dialog.get(forms::MOUSE) == "Enabled";
        self.review_required = dialog.get(forms::PLANNER_APPROVAL) == "Review required";
        self.cleanup_fail_once = dialog.get(CLEANUP) == "Fail next switch";
        self.clean_retry = dialog.get(RETRY) == "Clean after checkpoint";
        screen.auto_advance = dialog.get(CONTINUE) == "Enabled";
        self.tick_ms = delay;
        if dataset != "Keep" {
            (screen.queue, screen.history) = mock::dataset(&dataset);
            screen.queue_catalog = Catalog::default();
            screen.history_catalog = Catalog::default();
            screen.follow = true;
            screen.log_offset = 0;
            screen.events = vec!["Mock workspace reset".into()];
            screen.task_events = screen.events.clone();
            self.last_workspace = screen.current().map(|t| t.id);
        }
        screen.record("Configuration applied · session only");
        Ok(true)
    }

    fn config_form(&self, screen: &Screen) -> Dialog {
        let choice = |label: &str, choices: &[&str], on: bool| {
            Field::choice(label, choices, choices[usize::from(on)])
        };
        Dialog::new(
            DialogKind::Config,
            "Configuration",
            "Mouse and keyboard are supported equally.\nChanging Demo dataset resets this mock workspace only.",
            vec![
                forms::mouse(screen.mouse_enabled),
                Field::text(STEP_DELAY, &self.tick_ms.to_string()),
                Field::choice(
                    DATASET,
                    &["Keep", "Guided", "Empty", "Large", "Waits", "Approvals"],
                    "Keep",
                ),
                forms::planner_approval(self.review_required),
                choice(
                    CLEANUP,
                    &["Success", "Fail next switch"],
                    self.cleanup_fail_once,
                ),
                choice(
                    RETRY,
                    &["Keep useful work", "Clean after checkpoint"],
                    self.clean_retry,
                ),
                choice(CONTINUE, &["Disabled", "Enabled"], screen.auto_advance),
            ],
        )
    }

    /// One simulation step of the current task.
    fn simulate(&mut self, screen: &mut Screen) -> bool {
        if let Some(task) = screen.queue.first_mut()
            && task.waiting_for_limit()
        {
            let attempt = task.attempts.last_mut().expect("waiting attempt");
            if attempt.resume_at.is_some_and(|at| Instant::now() >= at) {
                attempt.resume_at = None;
                attempt.outcome = "Running".into();
                screen.record("Rate-limit wait ended · same attempt/session · no budget charged");
            }
            return true;
        }
        let Some(task) = screen
            .queue
            .first_mut()
            .filter(|t| t.status() == Status::Running)
        else {
            return false;
        };
        let attempt = task.attempts.last_mut().expect("running task has attempt");
        attempt.step += 1;
        attempt.elapsed_ms += self.tick_ms;
        attempt.cost_cents += 1;
        if attempt.revision.settings.timeout_ms > 0
            && attempt.elapsed_ms >= attempt.revision.settings.timeout_ms
        {
            attempt.state = Status::Failed;
            attempt.outcome = "Failed: active time limit reached".into();
            attempt.logs.push(attempt.outcome.clone());
            screen.record("Active time limit reached · r Retry restarts it; c Models raises it");
            return true;
        }
        if task.needs_rate_limit && attempt.step == 2 {
            attempt.resume_at = Some(Instant::now() + Duration::from_secs(3));
            attempt.outcome = "Waiting: rate limit (3 seconds)".into();
            attempt.logs.push(
                "RATE LIMIT: wait 3 seconds; retain attempt/session and active-time budget".into(),
            );
            screen.record("Provider rate limit · scheduled resume in 3 seconds (mock)");
            return true;
        }
        if task.needs_question && task.answer.is_empty() && attempt.step == 2 {
            attempt.state = Status::NeedsInput;
            attempt.outcome = "Waiting for answer".into();
            attempt.question = Some("Which export format should be used?".into());
            attempt
                .logs
                .push("QUESTION: Which export format should be used?".into());
            screen.record("Current task needs your answer");
            return true;
        }
        attempt.logs.push(
            match attempt.step {
                1 => "Executor: inspected the task",
                2 => "Executor: implementing",
                3 => "Checks: running",
                4 => "Review: checking the result",
                5 => "Validation: checking the goal",
                _ => "Checks: finishing",
            }
            .into(),
        );
        if attempt.step >= 6 {
            if task.id == 1 && !attempt.revision.repaired {
                attempt.state = Status::Failed;
                attempt.outcome = "Failed: boundary error".into();
                attempt.logs.push("FAIL: final row omitted.".into());
                screen.record("Check failed · later tasks remain pending");
            } else {
                self.complete_current(screen, "Completed; checks and review passed");
            }
        }
        true
    }
}

const STEP_ONE: &str = "Step 1 title";
const STEP_TWO: &str = "Step 2 title";

impl Backend for DemoWorkspace {
    fn act(&mut self, screen: &mut Screen, action: Action) {
        match action {
            Action::Run
                if screen
                    .current()
                    .is_some_and(|t| t.status() == Status::NeedsInput) =>
            {
                screen.dialog = Self::answer_form(screen);
            }
            Action::Run => self.start_current(screen),
            Action::Pause => {
                if let Some(task) = screen.queue.first_mut()
                    && let Some(attempt) = task.attempts.last_mut()
                {
                    attempt.state = Status::Paused;
                    attempt.outcome = "Paused".into();
                    attempt
                        .logs
                        .push("Paused by user; progress retained".into());
                }
                screen.record("Current paused · later tasks will not start");
            }
            Action::Approve => self.approve_selected(screen),
            _ => {}
        }
    }

    fn open(&mut self, screen: &mut Screen, action: Action) -> Result<Option<Dialog>, String> {
        let target = screen.target(action);
        Ok(match action {
            Action::Add | Action::Insert | Action::Edit => Some(forms::task(
                action,
                target.filter(|_| action == Action::Edit),
                "Title, goal, and acceptance check are required.\nBlank execution fields use defaults; with a new model, blank effort means none.",
            )),
            Action::Move => target.map(|task| forms::move_task(task.id)),
            Action::Reject => target.map(|task| forms::reject(task.id)),
            Action::Models => target.map(Self::models_form),
            Action::Stop
            | Action::Restart
            | Action::MarkDone
            | Action::CancelTask
            | Action::Supersede
            | Action::CopyToQueue => target.and_then(|task| forms::control(action, task)),
            Action::Breakdown => target.map(|task| {
                Dialog::new(
                    DialogKind::Breakdown(task.id),
                    "Break down task · capable model (mock)",
                    &format!(
                        "Replace #{} in place with the edited subtasks below.\nOriginal and attempts move to History. Cancel changes nothing.",
                        task.id
                    ),
                    vec![
                        Field::text(STEP_ONE, "Implement the focused change"),
                        Field::text(STEP_TWO, "Verify the acceptance checks"),
                    ],
                )
            }),
            Action::Answer => Self::answer_form(screen),
            Action::Help => Some(forms::help(
                "All tasks, models, runs, and files are simulated.",
            )),
            Action::Configure => Some(self.config_form(screen)),
            Action::Import => Some(Dialog::new(
                DialogKind::Import,
                "Import · in-memory demo files",
                "Available: /demo/import.tsv (valid), /demo/invalid.tsv\nOnly simulated files are read; no disk access.",
                vec![Field::text(forms::SOURCE, "/demo/import.tsv")],
            )),
            Action::Export => Some(Dialog::new(
                DialogKind::Export,
                "Export · in-memory demo files",
                "Writes to the session's virtual file shelf, NOT disk.\nUse the destination in Import to read it back.",
                vec![
                    Field::text(forms::DESTINATION, "/demo/export.tsv"),
                    forms::scope(screen.view),
                ],
            )),
            _ => None,
        })
    }

    fn apply(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        match dialog.kind.clone() {
            DialogKind::Task { id, insert } => self.save_task(screen, dialog, id, insert)?,
            DialogKind::Stop(_)
            | DialogKind::Restart(_)
            | DialogKind::MarkDone(_)
            | DialogKind::CancelTask(_)
            | DialogKind::Supersede(_)
            | DialogKind::CopyToQueue(_) => {
                let restart = matches!(dialog.kind, DialogKind::Restart(_));
                self.apply_control(screen, dialog)?;
                if restart {
                    self.start_current(screen);
                }
            }
            DialogKind::Move(id) => {
                let position = screen.insertion_position(dialog.get(forms::BEFORE), Some(id))?;
                let old = screen
                    .queue
                    .iter()
                    .position(|t| t.id == id && t.status() == Status::Pending)
                    .ok_or("Only pending work can move")?;
                let task = screen.queue.remove(old);
                screen.queue.insert(position, task);
                screen.reveal(id);
                screen.record(format!("#{id} moved · execution order updated"));
            }
            DialogKind::Models(id) => Self::save_models(screen, dialog, id)?,
            DialogKind::Reject(id) => Self::reject_revision(screen, dialog, id)?,
            DialogKind::Breakdown(id) => self.accept_breakdown(screen, dialog, id)?,
            DialogKind::Answer(id) => Self::answer(screen, dialog, id)?,
            DialogKind::Import => {
                let data = self
                    .files
                    .get(dialog.get(forms::SOURCE))
                    .ok_or("Mock file not found · try /demo/import.tsv")?;
                let tasks = parse_tasks(data)?;
                dialog.title = "Import preview · simulated".into();
                dialog.body = tasks
                    .iter()
                    .map(|t| format!("{} · {}", t.definition().kind.label(), t.definition().title))
                    .collect::<Vec<_>>()
                    .join("\n");
                dialog.kind = DialogKind::ImportPreview(tasks);
                dialog.fields = vec![Field::text(forms::BEFORE, "")];
                dialog.focus = 0;
                dialog.error.clear();
                return Ok(false);
            }
            DialogKind::ImportPreview(mut tasks) => {
                let position = screen.insertion_position(dialog.get(forms::BEFORE), None)?;
                for task in &mut tasks {
                    task.id = self.next_id;
                    self.next_id += 1;
                }
                let id = tasks[0].id;
                let count = tasks.len();
                screen.queue.splice(position..position, tasks);
                screen.reveal(id);
                screen.record(format!("Imported {count} tasks · simulated files only"));
            }
            DialogKind::Export => return self.export(screen, dialog),
            DialogKind::Overwrite { path, data } => {
                self.files.insert(path.clone(), data);
                screen.record(format!("Exported to {path} · overwrite confirmed (mock)"));
            }
            DialogKind::Config | DialogKind::ResetConfig => return self.configure(screen, dialog),
            DialogKind::Readiness
            | DialogKind::Help
            | DialogKind::Stats
            | DialogKind::Events
            | DialogKind::Inspect { .. } => {}
            DialogKind::BreakdownPending | DialogKind::BreakdownReview { .. } => {
                return Err("Persistent proposal is unavailable in demo mode".into());
            }
            DialogKind::ProjectModels(_)
            | DialogKind::UserDefaults(_)
            | DialogKind::ApplyDefaults(_)
            | DialogKind::Profiles(_)
            | DialogKind::Profile { .. }
            | DialogKind::Categories(_)
            | DialogKind::Category { .. }
            | DialogKind::Lenses(_)
            | DialogKind::Lens { .. }
            | DialogKind::LensDefaults(_)
            | DialogKind::ProjectChecks(_)
            | DialogKind::ProjectCheck { .. }
            | DialogKind::Preferences(..)
            | DialogKind::Delivery(_)
            | DialogKind::Storage(_)
            | DialogKind::StorageConfirm(_)
            | DialogKind::BuildCache(_)
            | DialogKind::BuildCacheConfirm(_)
            | DialogKind::StoredImport(_)
            | DialogKind::FileOverwrite { .. } => {
                return Err("Disk operations require SQLite mode".into());
            }
        }
        Ok(true)
    }

    fn stats(&self, screen: &Screen) -> Result<Dialog, String> {
        let tasks: Vec<_> = screen.queue.iter().chain(&screen.history).collect();
        let cost: usize = tasks.iter().map(|t| t.cost()).sum();
        let attempts = || tasks.iter().flat_map(|t| &t.attempts);
        let failed = attempts()
            .filter(|a| a.outcome.starts_with("Failed"))
            .count();
        let steps: usize = attempts().map(|a| a.step).sum();
        let elapsed_ms: u64 = attempts().map(|a| a.elapsed_ms).sum();
        let closed = |status| {
            screen
                .history
                .iter()
                .filter(|t| t.status() == status)
                .count()
        };
        Ok(Dialog::new(
            DialogKind::Stats,
            "Project statistics · simulated",
            &format!(
                "Tasks: {}\nQueue: {}\nHistory: {}\nCompleted: {}\nFailed attempts: {failed}\nExecution steps: {steps}\nElapsed execution: {:.1}s (simulated)\nTotal cost: {} (simulated)\nCancelled: {} · Superseded: {}",
                tasks.len(),
                screen.queue.len(),
                screen.history.len(),
                closed(Status::Done),
                elapsed_ms as f64 / 1000.0,
                money(cost),
                closed(Status::Cancelled),
                closed(Status::Superseded),
            ),
            vec![],
        ))
    }

    fn tick(&mut self, screen: &mut Screen) -> bool {
        if screen.last_tick.elapsed() < Duration::from_millis(self.tick_ms) {
            return false;
        }
        screen.last_tick = Instant::now();
        self.simulate(screen)
    }

    fn inspect_body(&self, screen: &Screen, dialog: &Dialog) -> String {
        let DialogKind::Inspect {
            id,
            attempt,
            section,
            step,
            ..
        } = dialog.kind
        else {
            return String::new();
        };
        let Some(task) = screen.task(id) else {
            return String::new();
        };
        let selected = task.attempts.get(attempt);
        let section = EvidenceSection::from_index(section);
        if matches!(
            section,
            EvidenceSection::Results
                | EvidenceSection::Transcripts
                | EvidenceSection::Reviews
                | EvidenceSection::Failures
        ) {
            return evidence_body(task, attempt, section, step);
        }
        let body = format!(
            "#{} {}\n{} · {}\n\nGoal: {}\nAcceptance check: {}\nTotal cost: {} (simulated)\nAnswer: {}\n\nAttempt {} / {}\n{}\n{}",
            task.id,
            task.definition().title,
            task.status().label(),
            task.definition().kind.label(),
            task.definition().goal,
            task.definition().check,
            money(task.cost()),
            if task.answer.is_empty() {
                "None"
            } else {
                &task.answer
            },
            if selected.is_some() { attempt + 1 } else { 0 },
            task.attempts.len(),
            selected.map_or("No attempts yet", |a| a.outcome.as_str()),
            selected.map_or(String::new(), |a| a.logs.join("\n"))
        );
        let pending = task.draft().map_or(String::new(), |r| {
            format!(
                "\nPending revision {}: {} (needs approval)",
                r.number, r.title
            )
        });
        let snapshot = selected.map_or(String::new(), |a| {
            format!(
                "\nAttempt inputs: revision {} · session {}\n{}\n{}",
                a.revision.number,
                a.session.map_or("not recorded".into(), |id| id.to_string()),
                a.outcome,
                a.revision.summary()
            )
        });
        format!(
            "{body}\n\n{}\nResolution: {}{pending}{snapshot}",
            task.definition().summary(),
            task.resolution_reason
        )
    }

    fn step_count(&self, screen: &Screen, at: EvidenceAt, reviews: bool) -> Result<usize, String> {
        Ok(screen
            .task(at.task)
            .and_then(|task| task.attempts.get(at.index))
            .map_or(0, |record| {
                record
                    .logs
                    .iter()
                    .filter(|line| step_line(line, reviews))
                    .count()
            }))
    }

    fn label(&self) -> &'static str {
        "MOCK DATA · session only"
    }

    fn information(&self, screen: &Screen) -> String {
        let on = |on: bool, yes: &'static str, no: &'static str| if on { yes } else { no };
        if screen.view == View::Config {
            format!(
                "Mouse: {}\nSimulation step delay: {} ms\nPlanner approval: {}\nContinue queue: {}\nSame-task retry: {}\nCleanup: {}\n\nDemo datasets: Large, Guided, Empty, Waits, Approvals.\nConfigure edits these session-only settings.\nKeyboard remains available with mouse disabled.",
                on(screen.mouse_enabled, "Enabled", "Disabled"),
                self.tick_ms,
                on(self.review_required, "Review required", "Automatic"),
                on(screen.auto_advance, "Enabled", "Disabled"),
                on(
                    self.clean_retry,
                    "Clean after checkpoint",
                    "Keep useful work"
                ),
                on(
                    self.cleanup_fail_once,
                    "Fail next switch (mock)",
                    "Success (mock)"
                ),
            )
        } else {
            format!(
                "{} · {}\nInstance: {}\n\nA visual workspace for supervising AI coding tasks.\n\nMOCK DATA: tasks, models, execution, and file operations\nare simulated; no backend, AI calls, or task CLI yet.\n\nDevelopment and production have separate app roots.\nUse --paths to inspect this instance's paths.",
                crate::PRODUCT,
                env!("CARGO_PKG_VERSION"),
                screen.instance.name()
            )
        }
    }

    fn config_controls(&self) -> Vec<(&'static str, Action)> {
        vec![
            ("e Configure", Action::Configure),
            ("Esc Back", Action::Back),
            ("S Stats", Action::Stats),
        ]
    }

    fn status_progress(&self, task: &Task) -> Vec<String> {
        let limit = task.definition().settings.attempt_limit;
        vec![
            task.attempts.last().map_or("No attempts yet".into(), |a| {
                format!("Attempt {} · step {}/6", task.attempts.len(), a.step)
            }),
            budget(task.used(), limit),
        ]
    }

    fn status_step(&self, _task: &Task) -> Option<String> {
        None
    }

    fn status_meter(&self, task: &Task, _width: usize, _compact: bool) -> Vec<String> {
        let model = &task.definition().settings.models[crate::presentation::active_role(task)];
        vec![
            format!(
                "Model {}",
                model_label(&model.name, model.effort.as_deref())
            ),
            format!("Cost {} (mock)", money(task.cost())),
        ]
    }

    fn running_situation(&self, _task: &Task) -> String {
        "Running (simulated)".into()
    }

    fn ready_situation(&self, _auto_advance: bool) -> &'static str {
        "Ready · r Run starts a simulated attempt"
    }

    fn idle_output(&self) -> &'static str {
        "Ready. Run current to begin a simulated attempt."
    }

    fn activity_title(&self, _task: usize) -> String {
        " Activity · l: all ".into()
    }

    fn history_scope(&self) -> &'static str {
        "read-only"
    }

    fn history_columns(&self) -> &'static [Column] {
        &[
            Column {
                name: "Attempts",
                left: false,
            },
            Column {
                name: "Time",
                left: false,
            },
            Column {
                name: "Usage",
                left: false,
            },
        ]
    }

    fn history_cells(&self, task: &Task) -> Vec<String> {
        vec![
            task.attempts.len().to_string(),
            duration(task.elapsed_ms()),
            if task.attempts.is_empty() {
                "-".into()
            } else {
                money(task.cost())
            },
        ]
    }

    fn task_cost(&self, task: &Task) -> String {
        money(task.cost())
    }

    fn attempt_lines(&self, _task: &Task) -> String {
        String::new()
    }
}

/// Whether a simulated log line belongs to the review (`reviews`) or failure steps.
fn step_line(line: &str, reviews: bool) -> bool {
    let lower = line.to_ascii_lowercase();
    if reviews {
        lower.contains("review")
    } else {
        lower.contains("fail") || lower.contains("error") || lower.contains("expected")
    }
}

/// A demo attempt's Results, Transcripts, Reviews or Failures from its simulated log.
fn evidence_body(task: &Task, attempt: usize, section: EvidenceSection, step: usize) -> String {
    let selected = task.attempts.get(attempt);
    let label = match section {
        EvidenceSection::Results => "Results",
        EvidenceSection::Transcripts => "Transcripts",
        EvidenceSection::Reviews => "Reviews",
        _ => "Failures",
    };
    let lines = selected.map_or(Vec::new(), |record| {
        record
            .logs
            .iter()
            .filter(|line| match section {
                EvidenceSection::Reviews => step_line(line, true),
                EvidenceSection::Failures => step_line(line, false),
                _ => true,
            })
            .collect::<Vec<_>>()
    });
    let detail = if section == EvidenceSection::Transcripts {
        "No transcripts in this demo fixture".to_owned()
    } else if lines.is_empty() {
        "No matching evidence recorded".to_owned()
    } else if matches!(
        section,
        EvidenceSection::Reviews | EvidenceSection::Failures
    ) {
        let shown = step.min(lines.len() - 1);
        format!("{} / {}\n{}", shown + 1, lines.len(), lines[shown])
    } else {
        lines.into_iter().cloned().collect::<Vec<_>>().join("\n")
    };
    format!(
        "#{} {} · demo\nAttempt {} / {} · {}\nOutcome: {}\n\n{label}\n{detail}",
        task.id,
        task.definition().title,
        attempt + 1,
        task.attempts.len(),
        selected.map_or("none", |a| a.state.label()),
        selected.map_or("No attempt", |a| a.outcome.as_str())
    )
}

/// Demo mode: move a task to History with `status`.
fn close(screen: &mut Screen, id: usize, status: Status, reason: &str) -> Result<(), String> {
    let action = match status {
        Status::Done => TaskAction::MarkDone,
        Status::Superseded => TaskAction::Supersede,
        _ => TaskAction::Cancel,
    };
    let index = screen
        .queue
        .iter()
        .position(|t| t.id == id && t.status().allows(action))
        .ok_or_else(|| format!("{} is no longer available for #{id}", action.label()))?;
    let mut task = screen.queue.remove(index);
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
    screen.history.insert(0, task);
    screen.history_catalog.selected = Some(id);
    screen.follow = true;
    screen.log_offset = 0;
    screen.record(format!("#{id} {} · {reason}", badge(status).to_lowercase()));
    Ok(())
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

fn parse_tasks(data: &str) -> Result<Vec<Task>, String> {
    let mut tasks = Vec::new();
    for (n, line) in data.lines().enumerate() {
        let parts: Vec<_> = line.split('\t').map(str::trim).collect();
        if !matches!(parts.len(), 4 | 14 | 15)
            || parts.iter().take(4).any(|s| s.is_empty())
            || Kind::from_label(parts[0]).is_none()
        {
            return Err(format!(
                "Invalid row {}: expected kind, title, goal, check (TSV)",
                n + 1
            ));
        }
        let mut task = Task::pending(0, parts[1]);
        task.revisions[0].goal = parts[2].into();
        task.revisions[0].check = parts[3].into();
        task.revisions[0].kind = Kind::from_label(parts[0]).expect("kind validated above");
        if parts.len() >= 14 {
            import_settings(&parts, &mut task)?;
        }
        tasks.push(task);
    }
    if tasks.is_empty() {
        return Err("Import contains no tasks".into());
    }
    Ok(tasks)
}

fn export_task(task: &Task) -> String {
    let revision = task.definition();
    let mut fields = vec![
        revision.kind.label().to_owned(),
        revision.title.clone(),
        revision.goal.clone(),
        revision.check.clone(),
    ];
    for model in &revision.settings.models {
        fields.push(model.name.clone());
        fields.push(model.effort.clone().unwrap_or_else(|| "none".into()));
    }
    fields.push(revision.settings.attempt_limit.to_string());
    fields.push(revision.settings.timeout_ms.to_string());
    fields.push(revision.settings.stage_timeout_ms.to_string());
    fields.join("\t")
}

fn import_settings(parts: &[&str], task: &mut Task) -> Result<(), String> {
    for (n, model) in task.revisions[0].settings.models.iter_mut().enumerate() {
        let name = parts[4 + 2 * n];
        let effort = parts[5 + 2 * n];
        if name.is_empty() || !matches!(effort, "none" | "low" | "medium" | "high" | "max") {
            return Err("Invalid imported model or effort".into());
        }
        model.name = name.into();
        model.effort = (effort != "none").then(|| effort.to_owned());
    }
    task.revisions[0].settings.attempt_limit = parts[12]
        .parse()
        .map_err(|_| "Invalid imported attempt limit")?;
    task.revisions[0].settings.timeout_ms = parts[13]
        .parse()
        .map_err(|_| "Invalid imported active time limit")?;
    task.revisions[0].settings.stage_timeout_ms = parts
        .get(14)
        .map_or(Ok(0), |stage| stage.parse())
        .map_err(|_| "Invalid imported stage time limit")?;
    Ok(())
}

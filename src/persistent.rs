//! The SQLite workspace: forms become application commands, then read projections reload.
//! No SQL, demo execution, or write-back of UI task objects lives here.
use crate::{
    app::{Action, EvidenceSection, Screen, View},
    application::{
        self as domain, Definition, EvidenceCursor, EvidenceSearch, ModelConfig, Service,
        VersionedTask, transfer,
    },
    backend::{Backend, Column, EvidenceAt},
    dialog::{Dialog, DialogKind, Field},
    forms,
    presentation::{
        self as model, Kind, ROLES, Status, Task, attempt_time, budget, duration, finished_label,
        model_label,
        task_text::{self, Detail},
    },
};
use std::{collections::BTreeMap, path::Path, time::Duration, time::Instant};

const APPROVAL: &str = "Approval";
const PROJECT_DEFAULTS: &str = "Project defaults";
const APPLY_DEFAULTS: &str = "Apply current project defaults";

pub(crate) struct StoredWorkspace {
    pub(crate) service: Service,
    /// The stored tasks the screen's projection came from, by id.
    pub(crate) stored: BTreeMap<usize, domain::Task>,
    proposal_job: Option<ProposalJob>,
    pending_control: Option<PendingControl>,
    /// Shown instead of "Saved to SQLite" once the saved form's change is committed.
    saved_notice: Option<String>,
    launch_issue: Option<String>,
    /// The task whose live supervisor run `readiness` was inspected during, if any.
    readiness_running: Option<usize>,
    pub(crate) review_required: bool,
}

struct ProposalJob {
    target: VersionedTask,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle:
        Option<std::thread::JoinHandle<Result<crate::runner::agent::BreakdownProposal, String>>>,
}

/// Stop or Mark done on a running task: the worker was asked to pause, and the control
/// completes once its runner has exited, without blocking the screen.
struct PendingControl {
    action: Action,
    id: usize,
}
impl Drop for ProposalJob {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl StoredWorkspace {
    pub fn open(screen: &mut Screen, service: Service) -> Result<Self, String> {
        let mut workspace = Self {
            service,
            stored: BTreeMap::new(),
            proposal_job: None,
            pending_control: None,
            saved_notice: None,
            launch_issue: None,
            readiness_running: None,
            review_required: false,
        };
        workspace.load_preferences(screen)?;
        workspace.reload(screen)?;
        screen.message(
            "Runs happen in a background supervisor; quitting this screen does not stop them",
        );
        Ok(workspace)
    }

    fn open_readiness(&mut self, screen: &mut Screen) {
        self.readiness_running = None;
        let report = match self.reload(screen) {
            Ok(()) => screen.readiness.clone().expect("reload inspects readiness"),
            Err(error) => crate::readiness::Readiness {
                lines: vec![format!(
                    "BLOCK · Refresh failed: {error} → Refresh before running"
                )],
                blockers: 1,
                refresh_failed: true,
            },
        };
        screen.readiness = Some(report.clone());
        let mut body = format!(
            "{} · {} blocker(s)\n\n{}",
            if report.blockers == 0 {
                "Can run"
            } else {
                "Run blocked"
            },
            report.blockers,
            report.lines.join("\n")
        );
        if let Some(issue) = &self.launch_issue
            && !report.refresh_failed
        {
            body.push_str(&format!("\n\nLast launch failure: {issue}"));
        }
        if let Some(session) = &screen.execution_session
            && !report.refresh_failed
            && session.state != "running"
            && session.state != "completed"
        {
            body.push_str(&format!(
                "\n\nQueue: {} · {}",
                session.state, session.reason
            ));
        }
        body.push_str("\n\nAfter fixing setup, Refresh here or press r on Current. Inspection creates no attempt.");
        screen.dialog = Some(Dialog::new(
            DialogKind::Readiness,
            "Execution readiness",
            &body,
            vec![],
        ));
    }

    fn usage(&self, id: usize) -> domain::usage_report::UsageTotals {
        self.stored
            .get(&id)
            .map(task_text::usage)
            .unwrap_or_default()
    }

    pub(crate) fn reload(&mut self, screen: &mut Screen) -> Result<(), String> {
        let service = &mut self.service;
        // A running attempt without a live runner was orphaned: settle it to Paused.
        if !crate::runner::supervisor_alive(service).map_err(|e| e.to_string())? {
            crate::runner::control::settle(service).map_err(|e| e.to_string())?;
            crate::runner::control::finish_pending(service).map_err(|e| e.to_string())?;
        }
        screen.execution_session = service.execution_session().map_err(|e| e.to_string())?;
        let snapshot = service.snapshot_brief().map_err(|e| e.to_string())?;
        let project = |tasks: &[domain::Task]| {
            tasks
                .iter()
                .map(crate::projection::project_task)
                .collect::<Result<Vec<_>, _>>()
        };
        let (queue, history) = (project(&snapshot.queue)?, project(&snapshot.history)?);
        let previous = screen.current().map(|t| t.id);
        self.stored = snapshot
            .queue
            .into_iter()
            .chain(snapshot.history)
            .map(|t| (t.id as usize, t))
            .collect();
        let alive = crate::runner::supervisor_alive(&self.service).map_err(|e| e.to_string())?;
        screen.supervisor_alive = Some(alive);
        screen.queue = queue;
        screen.history = history;
        self.launch_issue = match screen.current().map(|task| task.id as i64) {
            Some(id) => self
                .service
                .latest_launch_issue(id)
                .map_err(|e| e.to_string())?,
            None => None,
        };
        let current = screen.current().map(|t| t.id as i64);
        screen.task_events = snapshot
            .events
            .iter()
            .filter(|(task, _)| current.is_some() && (*task == current || task.is_none()))
            .map(|(_, message)| message.clone())
            .collect();
        screen.events = snapshot
            .events
            .into_iter()
            .map(|(_, message)| message)
            .collect();
        if screen.current().map(|t| t.id) != previous {
            screen.follow = true;
            screen.show_log = false;
            screen.log_offset = 0;
        }
        screen.reconcile();
        // While the supervisor runs the current attempt, nothing readiness checks can change,
        // and inspecting runs Git: inspect again once that run ends or another task is current.
        let running = screen
            .current()
            .filter(|task| alive && task.status() == Status::Running)
            .map(|task| task.id);
        if running.is_none() || running != self.readiness_running {
            screen.readiness = Some(crate::readiness::inspect(
                &self.service,
                screen
                    .current()
                    .and_then(|task| Some((task.status(), self.stored.get(&task.id)?))),
                alive,
            ));
        }
        self.readiness_running = running;
        Ok(())
    }

    fn target(&self, id: usize) -> Result<VersionedTask, String> {
        self.stored
            .get(&id)
            .map(versioned)
            .ok_or_else(|| "Task no longer exists in this view".into())
    }

    fn committed(&mut self, screen: &mut Screen, message: &str) {
        match self.reload(screen) {
            Ok(()) => screen.message(message),
            Err(error) => screen.message(format!("Saved to SQLite, but refresh failed: {error}")),
        }
    }

    /// Finish a Stop or Mark done that waited for the runner to stop its worker.
    fn poll_control(&mut self, screen: &mut Screen) -> bool {
        if self.pending_control.is_none() {
            return false;
        }
        if crate::runner::supervisor_alive(&self.service).unwrap_or(true) {
            return false;
        }
        if let Err(error) = crate::runner::control::finish_pending(&mut self.service) {
            self.pending_control.take();
            let _ = self.reload(screen);
            screen.message(error.to_string());
            return true;
        }
        let PendingControl { action, id } = self.pending_control.take().expect("pending");
        let result = self
            .service
            .pending_control()
            .map_err(|e| e.to_string())
            .and_then(|pending| {
                let pending = pending.ok_or_else(|| "Control receipt disappeared".to_owned())?;
                if !pending.completed || pending.task != id as i64 {
                    return Err("Control has not completed".into());
                }
                Ok(control_notice(id as i64, action, pending.stash))
            });
        match result {
            Ok(notice) => {
                self.committed(screen, &notice);
                if action == Action::MarkDone {
                    self.continue_queue(screen);
                    screen.message(notice);
                }
            }
            Err(error) => {
                let _ = self.reload(screen);
                screen.message(error);
            }
        }
        true
    }

    /// A committed intervention must not be presented as a failed form if starting fails.
    fn continue_queue(&mut self, screen: &mut Screen) {
        let result = screen
            .instance
            .paths()
            .map_err(|e| e.to_string())
            .and_then(|paths| {
                crate::runner::continue_after_intervention(&mut self.service, &paths)
                    .map_err(|e| e.to_string())
            });
        match result {
            Ok(true) => self.committed(screen, "Saved to SQLite · execution continuing"),
            Ok(false) => {
                if let Err(error) = self.reload(screen) {
                    screen.message(format!("Saved to SQLite, but refresh failed: {error}"));
                }
            }
            Err(error) => {
                self.committed(screen, &format!("Saved to SQLite · queue waiting: {error}"))
            }
        }
    }

    fn propose_breakdown(&mut self, screen: &Screen) -> Result<Dialog, String> {
        let id = screen.selected_task().ok_or("No selected task")?.id;
        let task = self
            .service
            .task(id as i64)
            .map_err(|e| e.to_string())?
            .ok_or("Task disappeared")?;
        let definition = task.definition().definition.clone();
        let model = self.service.project_defaults().map_err(|e| e.to_string())?[0]
            .clone()
            .ok_or("Configure a planning model first")?;
        self.service
            .validate_model("planning", &model)
            .map_err(|error| error.to_string())?;
        let paths = screen.instance.paths().map_err(|e| e.to_string())?;
        let cwd = self.service.project_root().map_err(|e| e.to_string())?;
        let target = self.target(id)?;
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancelled.clone();
        self.proposal_job = Some(ProposalJob {
            target,
            cancelled,
            handle: Some(std::thread::spawn(move || {
                crate::runner::agent::propose_breakdown(
                    &stop,
                    &model,
                    &definition.title,
                    &definition.goal,
                    &definition.criteria,
                    &paths,
                    &cwd,
                )
                .map_err(|e| e.to_string())
            })),
        });
        Ok(Dialog::new(
            DialogKind::BreakdownPending,
            "Requesting breakdown proposal",
            "Planning subtasks in execution order.\nCancel stops the request and changes nothing. Timeout: 120 seconds.",
            vec![],
        ))
    }

    fn poll_proposal(&mut self, screen: &mut Screen) -> bool {
        let Some(job) = &self.proposal_job else {
            return false;
        };
        if !matches!(
            screen.dialog.as_ref().map(|d| &d.kind),
            Some(DialogKind::BreakdownPending)
        ) {
            self.proposal_job.take();
            return true;
        }
        if !job.handle.as_ref().unwrap().is_finished() {
            return false;
        }
        let mut job = self.proposal_job.take().unwrap();
        let result = job
            .handle
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| Err("Breakdown worker panicked".into()));
        screen.dialog = None;
        match result {
            Ok(proposal) => screen.dialog = Some(breakdown_review(job.target, proposal)),
            Err(error) => screen.message(error),
        }
        true
    }

    /// Reload when another process changed the store; redraw while the current task lives.
    fn refresh(&mut self, screen: &mut Screen) -> bool {
        match self.service.externally_changed() {
            Ok(true) => {
                if let Err(error) = self.reload(screen) {
                    screen.message(error);
                }
            }
            Ok(false) => {
                if screen.current().is_some_and(|t| {
                    matches!(
                        t.status(),
                        Status::Running | Status::Paused | Status::NeedsInput
                    )
                }) {
                    // Nothing stored changed, so only the supervisor can have started or
                    // stopped; redraw anyway so timers and countdowns advance.
                    let unchanged = crate::runner::supervisor_alive(&self.service)
                        .is_ok_and(|alive| Some(alive) == screen.supervisor_alive);
                    if !unchanged && let Err(error) = self.reload(screen) {
                        screen.message(error);
                    }
                    return true;
                }
                return false;
            }
            Err(error) => screen.message(error.to_string()),
        }
        true
    }

    fn run(&mut self, screen: &mut Screen) {
        if screen.current().is_some_and(Task::awaits_answer) {
            match self.open(screen, Action::Answer) {
                Ok(dialog) => screen.dialog = dialog,
                Err(error) => screen.message(error),
            }
            return;
        }
        let resume = screen
            .current()
            .is_some_and(|task| matches!(task.status(), Status::Paused | Status::NeedsInput));
        let queue = if resume {
            screen
                .execution_session
                .as_ref()
                .map_or(screen.auto_advance, |session| session.mode == "queue")
        } else {
            screen.auto_advance
        };
        let result = screen
            .instance
            .paths()
            .map_err(|e| e.to_string())
            .and_then(|paths| {
                crate::runner::background(&mut self.service, &paths, queue, false)
                    .map_err(|e| e.to_string())
            });
        let current = screen.current().map(|task| task.id as i64);
        match result {
            Ok(()) => {
                if let Some(id) = current {
                    let _ = self.service.record_launch_result(id, "");
                }
                self.committed(
                    screen,
                    "Supervisor started · close TUI safely · Pause stops execution",
                );
            }
            Err(error) => {
                if let Some(id) = current {
                    let _ = self.service.record_launch_result(id, &error);
                    let _ = self.reload(screen);
                }
                screen.message(error);
            }
        }
    }

    fn task_form(&self, screen: &Screen, action: Action) -> Dialog {
        let task = screen.target(action).filter(|_| action == Action::Edit);
        let mut dialog = forms::task(
            action,
            task,
            if task.is_none() {
                "Save creates a task in SQLite. Blank execution fields use project defaults; with a new model, blank effort means none.\nNeeds review creates a draft."
            } else {
                "Save commits a new revision to SQLite. Earlier attempts remain unchanged.\nNeeds review creates a draft; any previous ready revision stays usable."
            },
        );
        let stored = task.and_then(|task| self.stored.get(&task.id));
        let ready = stored.is_none_or(|t| t.definition().lifecycle == "ready");
        dialog.fields.push(Field::choice(
            APPROVAL,
            &["Ready", "Needs review"],
            if ready { "Ready" } else { "Needs review" },
        ));
        let current = stored.and_then(|t| t.definition().definition.category.clone());
        let mut names = vec![forms::DEFAULT_CATEGORY.to_owned()];
        names.extend(
            self.service
                .categories()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| entry.category.name),
        );
        if let Some(name) = current.as_ref().filter(|n| !names.contains(n)) {
            names.push(name.clone());
        }
        dialog.fields.push(Field::choice(
            forms::CATEGORY,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            current.as_deref().unwrap_or(forms::DEFAULT_CATEGORY),
        ));
        dialog.expected = stored.map(versioned);
        dialog
    }

    fn models_form(&self, task: &Task) -> Result<Dialog, String> {
        let stored = self
            .stored
            .get(&task.id)
            .ok_or("Task no longer available")?;
        let revision = models_revision(stored);
        let mut fields = Vec::new();
        for (role, model) in ROLES.iter().zip(&revision.definition.models) {
            fields.push(Field::text(
                &format!("{role} provider/model"),
                &model
                    .as_ref()
                    .map_or(String::new(), ModelConfig::identifier),
            ));
            fields.push(Field::text(
                &format!("{role} effort (blank = none)"),
                model
                    .as_ref()
                    .and_then(|m| m.effort.as_deref())
                    .unwrap_or(""),
            ));
        }
        fields.extend(forms::limit_fields(
            &task.draft().unwrap_or_else(|| task.definition()).settings,
        ));
        fields.push(Field::choice(
            PROJECT_DEFAULTS,
            &["Keep task settings", APPLY_DEFAULTS],
            "Keep task settings",
        ));
        let defaults = self
            .service
            .defaults()
            .unwrap_or(std::array::from_fn(|_| None));
        let mut dialog = Dialog::new(
            DialogKind::Models(task.id),
            "Task models and limits",
            &format!(
                "Use provider/model: codex/…, claude/sonnet, opencode/vendor/model; blank means unconfigured.\nApply current project defaults replaces all four role settings on Save. Effective defaults preview:\n{}\nSave approves a new revision.",
                defaults_preview(&defaults)
            ),
            fields,
        );
        dialog.expected = Some(versioned(stored));
        dialog.field_limit = Some(7);
        Ok(dialog)
    }

    fn with_version(&self, mut dialog: Dialog, id: usize) -> Dialog {
        dialog.expected = self.stored.get(&id).map(versioned);
        dialog
    }

    fn answer_form(&self, screen: &Screen) -> Result<Option<Dialog>, String> {
        let Some(id) = screen.current().map(|t| t.id) else {
            return Ok(None);
        };
        match self
            .service
            .open_question(id as i64)
            .map_err(|e| e.to_string())?
        {
            Some((_, question)) => Ok(Some(self.with_version(forms::answer(id, &question), id))),
            None => Err("No open question; refresh the task".into()),
        }
    }

    fn apply_breakdown(
        &mut self,
        screen: &mut Screen,
        dialog: &Dialog,
        proposal: &crate::runner::agent::BreakdownProposal,
    ) -> Result<bool, String> {
        let target = dialog
            .expected
            .ok_or("Missing task version; reopen the form")?;
        let paths = screen.instance.paths().map_err(|e| e.to_string())?;
        let ready = self
            .service
            .preferences(&paths.config)
            .map_err(|e| e.to_string())?
            .planner_approval
            == domain::preferences::PlannerApproval::Automatic;
        let defaults = self.service.defaults().map_err(|e| e.to_string())?;
        let parent = &self
            .stored
            .get(&(target.id as usize))
            .ok_or("Task no longer available")?
            .definition()
            .definition;
        let definitions = crate::runner::agent::breakdown_definitions(proposal, defaults, parent);
        let ids = self
            .service
            .apply_breakdown(target, &definitions, &proposal.reason, ready)
            .map_err(|e| e.to_string())?;
        self.committed(screen, "Saved to SQLite");
        screen.reveal(ids[0] as usize);
        Ok(true)
    }

    fn control(
        &mut self,
        dialog: &Dialog,
        action: Action,
        id: usize,
        target: VersionedTask,
    ) -> Result<(), String> {
        let reason = dialog.get(forms::REASON).to_owned();
        let running = self
            .stored
            .get(&id)
            .is_some_and(|t| t.state() == domain::lifecycle::State::Running)
            && crate::runner::supervisor_alive(&self.service).map_err(|e| e.to_string())?;
        if running {
            self.service
                .request_control(
                    target,
                    if action == Action::Stop {
                        "stop"
                    } else {
                        "done"
                    },
                    &reason,
                )
                .map_err(|e| e.to_string())?;
            self.pending_control = Some(PendingControl { action, id });
            self.saved_notice = Some(format!(
                "#{id}: stopping the worker · {} completes when it exits",
                if action == Action::Stop {
                    "Stop"
                } else {
                    "Mark done"
                }
            ));
        } else {
            let saved = if action == Action::Stop {
                crate::runner::control::stop(&mut self.service, target, &reason)
            } else {
                crate::runner::control::mark_done(&mut self.service, target, &reason)
            }
            .map_err(|e| e.to_string())?;
            self.saved_notice = Some(control_notice(target.id, action, saved.stash));
        }
        Ok(())
    }

    fn save_task(
        &mut self,
        screen: &mut Screen,
        dialog: &Dialog,
        id: Option<usize>,
        insert: bool,
        target: impl Fn() -> Result<VersionedTask, String>,
    ) -> Result<(), String> {
        let mut definition = if let Some(id) = id {
            self.stored
                .get(&id)
                .ok_or("Task no longer available")?
                .definition()
                .definition
                .clone()
        } else {
            Definition {
                title: String::new(),
                goal: String::new(),
                kind: domain::Kind::Ai,
                category: None,
                criteria: Vec::new(),
                models: self.service.defaults().map_err(|e| e.to_string())?,
                attempt_limit: Some(3),
                timeout_ms: None,
                stage_timeout_ms: None,
            }
        };
        definition.title = dialog.get(forms::TITLE).into();
        definition.goal = dialog.get(forms::GOAL).into();
        let criteria = dialog.get(forms::CHECK);
        if definition.criteria.join("\n") != criteria {
            definition.criteria = criteria.lines().map(str::to_owned).collect();
        }
        let previous = (definition.kind, definition.category.clone());
        definition.kind = match Kind::from_label(dialog.get(forms::KIND)) {
            Some(Kind::Human) => domain::Kind::Human,
            Some(Kind::Docs) => domain::Kind::Docs,
            _ => domain::Kind::Ai,
        };
        definition.category =
            chosen_category(dialog.get(forms::CATEGORY), &previous, definition.kind)?;
        let ready = dialog.get(APPROVAL) == "Ready";
        let selected = if let Some(id) = id {
            self.service
                .revise(target()?, &definition, ready)
                .map_err(|e| e.to_string())?;
            id
        } else {
            set_new_task_execution_model(
                &mut definition,
                dialog.get(forms::MODEL),
                dialog.get(forms::EFFORT),
            )?;
            let before = if insert {
                before_id(dialog.get(forms::BEFORE))?
            } else {
                None
            };
            self.service
                .create(&[definition], before, ready)
                .map_err(|e| e.to_string())?[0] as usize
        };
        screen.queue_catalog.selected = Some(selected);
        if id.is_none() {
            screen.view = View::Queue;
            screen.queue_catalog.query.clear();
        }
        Ok(())
    }

    fn save_models(
        &mut self,
        dialog: &Dialog,
        id: usize,
        target: VersionedTask,
    ) -> Result<(), String> {
        let task = self.stored.get(&id).ok_or("Task no longer available")?;
        let mut definition = models_revision(task).definition.clone();
        let apply_defaults = dialog.get(PROJECT_DEFAULTS) == APPLY_DEFAULTS;
        if !apply_defaults {
            for (role, model) in ROLES.iter().zip(definition.models.iter_mut()) {
                *model = ModelConfig::from_fields(
                    dialog.get(&format!("{role} provider/model")),
                    dialog.get(&format!("{role} effort (blank = none)")),
                )?;
            }
        }
        definition.attempt_limit = limit(dialog.get(forms::ATTEMPT_LIMIT))?;
        definition.timeout_ms = domain::limits::parse(dialog.get(forms::TASK_TIME))?;
        definition.stage_timeout_ms = domain::limits::parse(dialog.get(forms::STAGE_TIME))?;
        if apply_defaults {
            let defaults = self.service.defaults().map_err(|e| e.to_string())?;
            if !dialog.body.contains(&defaults_preview(&defaults)) {
                return Err("Project defaults changed; reopen Models to preview them again".into());
            }
            definition.models = defaults;
        }
        self.service
            .revise(target, &definition, true)
            .map_err(|e| e.to_string())
    }

    fn export(&self, screen: &Screen, dialog: &mut Dialog) -> Result<bool, String> {
        let tasks: Vec<_> = match dialog.get(forms::SCOPE) {
            "History" => screen.history.iter().collect(),
            "All" => screen.queue.iter().chain(&screen.history).collect(),
            _ => screen.queue.iter().collect(),
        };
        let definitions = tasks
            .iter()
            .map(|t| self.stored[&t.id].definition().definition.clone())
            .collect::<Vec<_>>();
        let data = transfer::encode(&definitions).map_err(|e| e.to_string())?;
        let path = dialog.get(forms::DESTINATION).to_owned();
        if Path::new(&path).symlink_metadata().is_ok() {
            let expected = transfer::read(Path::new(&path)).map_err(|e| e.to_string())?;
            dialog.title = "Overwrite file?".into();
            dialog.body = format!("{path}\nConfirm replaces this file. Cancel keeps its contents.");
            dialog.kind = DialogKind::FileOverwrite {
                path,
                data,
                expected,
            };
            dialog.fields.clear();
            dialog.error.clear();
            return Ok(false);
        }
        transfer::export(Path::new(&path), &data, None).map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Import or export task definitions; Ok(false) shows the preview or overwrite step.
    fn transfer(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        match dialog.kind.clone() {
            DialogKind::Import => {
                let definitions = transfer::import(Path::new(dialog.get(forms::SOURCE)))
                    .map_err(|e| e.to_string())?;
                dialog.title = "Import preview · SQLite".into();
                dialog.body = definitions
                    .iter()
                    .map(|d| format!("{} · {}", d.kind.as_str(), d.title))
                    .collect::<Vec<_>>()
                    .join("\n");
                dialog.kind = DialogKind::StoredImport(definitions);
                dialog.fields = vec![Field::text(forms::BEFORE, "")];
                dialog.focus = 0;
                dialog.error.clear();
                return Ok(false);
            }
            DialogKind::StoredImport(definitions) => {
                let ids = self
                    .service
                    .import(&definitions, before_id(dialog.get(forms::BEFORE))?)
                    .map_err(|e| e.to_string())?;
                screen.queue_catalog.selected = Some(ids[0] as usize);
                screen.queue_catalog.query.clear();
            }
            DialogKind::Export => return self.export(screen, dialog),
            DialogKind::FileOverwrite {
                path,
                data,
                expected,
            } => transfer::export(Path::new(&path), &data, Some(&expected))
                .map_err(|e| e.to_string())?,
            _ => return Err("Not an import or export form".into()),
        }
        Ok(true)
    }

    /// Apply a task form; Ok(false) keeps its next step open.
    fn execute_form(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        let expected = dialog.expected;
        let target = || expected.ok_or_else(|| "Missing task version; reopen the form".to_owned());
        match dialog.kind.clone() {
            DialogKind::BreakdownPending => return Ok(false),
            DialogKind::Stop(id) => self.control(dialog, Action::Stop, id, target()?)?,
            DialogKind::MarkDone(id) => self.control(dialog, Action::MarkDone, id, target()?)?,
            DialogKind::CancelTask(_) => self
                .service
                .cancel(target()?, dialog.get(forms::REASON))
                .map_err(|e| e.to_string())?,
            DialogKind::Supersede(_) => {
                let by = forms::task_ids(dialog.get(forms::REPLACED_BY))?
                    .into_iter()
                    .map(|id| id as i64)
                    .collect::<Vec<_>>();
                self.service
                    .supersede(target()?, &by, dialog.get(forms::REASON))
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::CopyToQueue(_) => {
                let before = before_id(dialog.get(forms::COPY_BEFORE))?;
                let id = self
                    .service
                    .copy_to_queue(target()?, before)
                    .map_err(|e| e.to_string())?;
                screen.view = View::Queue;
                screen.queue_catalog.query.clear();
                screen.queue_catalog.filter = 0;
                screen.queue_catalog.selected = Some(id as usize);
                self.saved_notice = Some(format!("Copied to the queue as #{id}"));
            }
            DialogKind::Answer(_) => self
                .service
                .answer(target()?, dialog.get(forms::ANSWER))
                .map_err(|e| e.to_string())?,
            DialogKind::Task { id, insert } => {
                self.save_task(screen, dialog, id, insert, target)?
            }
            DialogKind::Models(id) => self.save_models(dialog, id, target()?)?,
            DialogKind::Move(_) => self
                .service
                .move_before(target()?, before_id(dialog.get(forms::BEFORE))?)
                .map_err(|e| e.to_string())?,
            DialogKind::Reject(_) => self
                .service
                .decide(target()?, false, dialog.get(forms::REJECT_REASON))
                .map_err(|e| e.to_string())?,
            DialogKind::Import
            | DialogKind::StoredImport(_)
            | DialogKind::Export
            | DialogKind::FileOverwrite { .. } => return self.transfer(screen, dialog),
            DialogKind::Help
            | DialogKind::Stats
            | DialogKind::Events
            | DialogKind::Inspect { .. } => return Ok(true),
            DialogKind::Readiness
            | DialogKind::Config
            | DialogKind::ResetConfig
            | DialogKind::Breakdown(_)
            | DialogKind::ImportPreview(_)
            | DialogKind::Overwrite { .. }
            | DialogKind::BreakdownReview { .. }
            | DialogKind::ProjectModels(_)
            | DialogKind::UserDefaults(_)
            | DialogKind::ApplyDefaults(_)
            | DialogKind::Profiles(_)
            | DialogKind::Profile { .. }
            | DialogKind::Categories(_)
            | DialogKind::Category { .. }
            | DialogKind::ProjectChecks(_)
            | DialogKind::ProjectCheck { .. }
            | DialogKind::Preferences(_)
            | DialogKind::Delivery(_)
            | DialogKind::Storage(_)
            | DialogKind::StorageConfirm(_)
            | DialogKind::BuildCache(_)
            | DialogKind::BuildCacheConfirm(_) => {
                return Err("Simulation is available only with --demo".into());
            }
        }
        Ok(true)
    }

    fn inspect_attempt(
        &self,
        screen: &Screen,
        dialog: &Dialog,
        task: &domain::Task,
        record: &domain::Attempt,
    ) -> String {
        let DialogKind::Inspect {
            attempt,
            section,
            page,
            chunk,
            artifact,
            step,
            match_sequence,
            watermark,
            ref excerpt,
            ..
        } = dialog.kind
        else {
            return String::new();
        };
        let service = &self.service;
        let evidence = match section {
            0 => service.browse_output_at(
                record.id,
                page,
                chunk,
                match_sequence.and_then(|sequence| {
                    dialog
                        .fields
                        .first()
                        .map(|field| (sequence, field.value.as_str()))
                }),
                watermark,
            ),
            1 => service.browse_results(record.id),
            2 => screen
                .instance
                .paths()
                .map_err(Into::into)
                .and_then(|paths| {
                    service.browse_transcript(record.id, artifact, page, &paths.state)
                }),
            3 => service.browse_step_outline(record.id),
            4 => service.browse_step_group(record.id, true, step),
            _ => service.browse_step_group(record.id, false, step),
        }
        .unwrap_or_else(|error| format!("Evidence unavailable: {error}"));
        let outcome = record
            .outcome
            .as_deref()
            .unwrap_or("No outcome recorded")
            .lines()
            .next()
            .unwrap_or_default();
        let heading = format!(
            "Attempt {} / {} · #{} · revision {} · {}\nOutcome: {outcome}",
            attempt + 1,
            task.attempts.len(),
            record.id,
            record.revision,
            record.state,
        );
        if section >= 3 {
            let reason = if section == 5 {
                format!(
                    "Failure category: {}\nReason: {}\n",
                    record.failure_category.as_deref().unwrap_or("not recorded"),
                    record
                        .report
                        .as_deref()
                        .unwrap_or("No attempt report recorded")
                )
            } else {
                String::new()
            };
            return format!(
                "#{} {}\n{heading}\n{reason}\n{evidence}",
                task.id,
                task.definition().definition.title,
            );
        }
        let revision = task
            .revisions
            .iter()
            .find(|revision| revision.number == record.revision)
            .unwrap_or_else(|| task.definition());
        format!(
            "{heading}\n{}{}{evidence}\n\nAttempt inputs: revision {} · session {}\n{}\nActive {} · waiting {}\nUsage: {}\n{}\n\n{}",
            record
                .report
                .as_deref()
                .filter(|report| Some(*report) != record.outcome.as_deref())
                .map_or(String::new(), |report| format!(
                    "Supervisor outcome: {}\n",
                    report.lines().next().unwrap_or_default()
                )),
            if excerpt.is_empty() {
                String::new()
            } else {
                format!("Match: {excerpt}\n")
            },
            record.revision,
            record
                .session
                .map_or("not recorded".into(), |id| id.to_string()),
            task_text::revision(revision, str::to_owned),
            domain::limits::elapsed(record.active_ms),
            domain::limits::elapsed(record.waiting_ms),
            record.usage.label(),
            record
                .role_usage
                .iter()
                .map(task_text::role_usage)
                .collect::<Vec<_>>()
                .join("\n"),
            task_text::task(task, Detail::Overview, str::to_owned).trim_end(),
        )
    }
}

impl Backend for StoredWorkspace {
    fn act(&mut self, screen: &mut Screen, action: Action) {
        let result = match action {
            Action::Readiness => {
                self.open_readiness(screen);
                return;
            }
            Action::Run => {
                self.run(screen);
                return;
            }
            Action::Pause => {
                let id = screen.current().expect("enabled current").id;
                self.target(id).and_then(|target| {
                    self.service
                        .request_pause(target)
                        .map_err(|e| e.to_string())
                })
            }
            Action::Approve => {
                let id = screen.selected_task().expect("enabled task").id;
                self.target(id).and_then(|target| {
                    self.service
                        .decide(target, true, "Approved by owner")
                        .map_err(|e| e.to_string())
                })
            }
            _ => return,
        };
        match result {
            Ok(()) => {
                let continue_queue = action == Action::Approve
                    && screen.selected_task().is_some_and(|selected| {
                        screen
                            .current()
                            .is_some_and(|current| current.id == selected.id)
                    });
                self.committed(
                    screen,
                    if action == Action::Pause {
                        "Pause requested · waiting for worker shutdown and cleanup"
                    } else {
                        "Saved to SQLite"
                    },
                );
                if continue_queue {
                    self.continue_queue(screen);
                }
            }
            Err(error) => screen.message(error),
        }
    }

    fn open(&mut self, screen: &mut Screen, action: Action) -> Result<Option<Dialog>, String> {
        let target = screen.target(action);
        Ok(match action {
            Action::Configure
            | Action::ProjectModels
            | Action::UserDefaults
            | Action::Profiles
            | Action::Categories
            | Action::ProjectChecks
            | Action::Delivery
            | Action::Storage
            | Action::BuildCache => Some(self.open_project_config(screen, action)?),
            Action::Breakdown => Some(self.propose_breakdown(screen)?),
            Action::Add | Action::Insert | Action::Edit => Some(self.task_form(screen, action)),
            Action::Move => {
                target.map(|task| self.with_version(forms::move_task(task.id), task.id))
            }
            Action::Reject => target.map(|task| self.with_version(forms::reject(task.id), task.id)),
            Action::Models => target.map(|task| self.models_form(task)).transpose()?,
            Action::Stop
            | Action::MarkDone
            | Action::CancelTask
            | Action::Supersede
            | Action::CopyToQueue => target.and_then(|task| {
                forms::control(action, task).map(|dialog| self.with_version(dialog, task.id))
            }),
            Action::Answer => self.answer_form(screen)?,
            Action::Help => Some(forms::help(
                "Changes save immediately. r runs the current task in a background supervisor; p pauses it.\nr on a paused task resumes the same attempt; on a failed task it retries, keeping the code and restarting the limits.\ns Stop returns any live task to Pending and stashes its uncommitted changes; the next Run starts fresh.\nD marks any unfinished task done. Pending tasks can be cancelled (x) or superseded (R); History offers C Copy to queue.\nChecks gate every result; review and validation judge it once the checks pass.",
            )),
            Action::Import => Some(Dialog::new(
                DialogKind::Import,
                "Import task definitions · disk",
                "Read a ktask-tasks-v3 (or v2, v1) TSV export. Preview before committing.\nImports create new tasks, not copied attempt/history records.",
                vec![Field::text(forms::SOURCE, "tasks.tsv")],
            )),
            Action::Export => Some(Dialog::new(
                DialogKind::Export,
                "Export task definitions · disk",
                "Versioned TSV includes criteria, provider/model/effort and limits.\nThis is a definition export, not a history backup.",
                vec![
                    Field::text(forms::DESTINATION, "tasks.tsv"),
                    forms::scope(screen.view),
                ],
            )),
            _ => None,
        })
    }

    fn apply(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        if let DialogKind::BreakdownReview { proposal } = &dialog.kind {
            let proposal = proposal.clone();
            return self.apply_breakdown(screen, dialog, &proposal);
        }
        if matches!(
            dialog.kind,
            DialogKind::ProjectModels(_)
                | DialogKind::UserDefaults(_)
                | DialogKind::ApplyDefaults(_)
                | DialogKind::Profiles(_)
                | DialogKind::Profile { .. }
                | DialogKind::Categories(_)
                | DialogKind::Category { .. }
                | DialogKind::ProjectChecks(_)
                | DialogKind::ProjectCheck { .. }
                | DialogKind::Preferences(..)
                | DialogKind::Delivery(_)
                | DialogKind::Storage(_)
                | DialogKind::StorageConfirm(_)
                | DialogKind::BuildCache(_)
                | DialogKind::BuildCacheConfirm(_)
        ) {
            return self.save_project_config(screen, dialog);
        }
        let readonly = dialog.read_only();
        let exporting = matches!(
            dialog.kind,
            DialogKind::Export | DialogKind::FileOverwrite { .. }
        );
        let result = self.execute_form(screen, dialog)?;
        if result && !readonly {
            self.committed(
                screen,
                if exporting {
                    "Exported to disk"
                } else {
                    "Saved to SQLite"
                },
            );
            if let Some(notice) = self.saved_notice.take() {
                screen.message(notice);
            }
            if matches!(dialog.kind, DialogKind::Answer(_))
                || (matches!(dialog.kind, DialogKind::MarkDone(_))
                    && self.pending_control.is_none())
            {
                self.continue_queue(screen);
            }
        }
        Ok(result)
    }

    fn stats(&self, screen: &Screen) -> Result<Dialog, String> {
        let usage = self.service.project_usage().map_err(|e| e.to_string())?;
        let timing = self.service.project_timing().map_err(|e| e.to_string())?;
        Ok(Dialog::new(
            DialogKind::Stats,
            "Project statistics · SQLite",
            &format!(
                "Queue: {}\nHistory: {}\nAttempts: {} ({} failed/interrupted)\nActive time: {}\nWaiting time: {}\nProject usage: {}\nHuman tasks are marked done without AI attempts.",
                screen.queue.len(),
                screen.history.len(),
                timing.attempts,
                timing.failed,
                domain::limits::elapsed(timing.active_ms),
                domain::limits::elapsed(timing.waiting_ms),
                usage.label()
            ),
            vec![],
        ))
    }

    fn tick(&mut self, screen: &mut Screen) -> bool {
        if self.poll_proposal(screen) || self.poll_control(screen) {
            return true;
        }
        if screen.dialog.is_some() || screen.last_tick.elapsed() < Duration::from_millis(500) {
            return false;
        }
        let changed = self.refresh(screen);
        // A slow snapshot must not make every queued input trigger another
        // snapshot. Leave an input-processing interval after refresh finishes.
        screen.last_tick = Instant::now();
        changed
    }

    fn inspect_body(&self, screen: &Screen, dialog: &Dialog) -> String {
        let DialogKind::Inspect { id, attempt, .. } = dialog.kind else {
            return String::new();
        };
        let Some(task) = self.stored.get(&id) else {
            return String::new();
        };
        match task.attempts.get(attempt) {
            Some(record) => self.inspect_attempt(screen, dialog, task, record),
            None => format!(
                "{}\nNo attempt evidence recorded.",
                task_text::task(task, Detail::Overview, str::to_owned).trim_end()
            ),
        }
    }

    fn find_label(&self, section: EvidenceSection) -> Option<&'static str> {
        match section {
            EvidenceSection::Output => Some("Find output"),
            EvidenceSection::Results => Some("Find results"),
            EvidenceSection::Transcripts => Some("Find transcript"),
            _ => None,
        }
    }

    fn watermark(&self, attempt: i64) -> Result<i64, String> {
        self.service
            .output_watermark(attempt)
            .map_err(|e| e.to_string())
    }

    fn artifact_id(&self, attempt: i64, index: usize) -> Option<i64> {
        self.service.artifact_id(attempt, index).ok().flatten()
    }

    fn artifact_count(&self, attempt: i64) -> Result<usize, String> {
        self.service
            .artifact_count(attempt)
            .map_err(|e| e.to_string())
    }

    fn page_count(
        &self,
        screen: &Screen,
        at: EvidenceAt,
        section: EvidenceSection,
        artifact: usize,
    ) -> Result<usize, String> {
        if section == EvidenceSection::Output {
            self.service.output_page_count_at(at.attempt, at.watermark)
        } else {
            screen
                .instance
                .paths()
                .map_err(Into::into)
                .and_then(|paths| {
                    self.service
                        .transcript_page_count(at.attempt, artifact, &paths.state)
                })
        }
        .map_err(|e| e.to_string())
    }

    fn chunk_count(&self, at: EvidenceAt, page: usize) -> Result<usize, String> {
        self.service
            .output_chunk_count_at(at.attempt, page, at.watermark)
            .map_err(|e| e.to_string())
    }

    fn step_count(&self, _screen: &Screen, at: EvidenceAt, reviews: bool) -> Result<usize, String> {
        self.service
            .step_group_count(at.attempt, reviews)
            .map_err(|e| e.to_string())
    }

    fn search(
        &self,
        screen: &Screen,
        at: EvidenceAt,
        section: EvidenceSection,
        artifact: usize,
        query: &str,
        cursor: EvidenceCursor,
    ) -> Result<EvidenceSearch, String> {
        match section {
            EvidenceSection::Output => {
                self.service
                    .search_output(at.attempt, at.watermark, query, cursor)
            }
            EvidenceSection::Results => self.service.search_results(at.attempt, query, cursor),
            _ => screen
                .instance
                .paths()
                .map_err(Into::into)
                .and_then(|paths| {
                    self.service.search_transcript(
                        at.attempt,
                        artifact,
                        &paths.state,
                        query,
                        cursor,
                    )
                }),
        }
        .map_err(|e| e.to_string())
    }

    fn label(&self) -> &'static str {
        "SQLite · real tasks"
    }

    fn information(&self, screen: &Screen) -> String {
        if screen.view != View::Config {
            return format!(
                "{} · {} · {}\nA visual workspace for supervising AI coding tasks.\n\nTasks, revisions, approvals, history and events:\nrelational SQLite. Import/export uses real files.\nExecution, checks, review, validation, remediation and task breakdown are real,\nrun by Codex, Claude Code or OpenCode per role.\n\nProduction and development use separate data roots.\n--demo never opens SQLite. --paths shows data locations.",
                crate::PRODUCT,
                env!("CARGO_PKG_VERSION"),
                screen.instance.name()
            );
        }
        let delivery = self.service.delivery_config().unwrap_or_default();
        format!(
            "Project: {}\nStorage: SQLite output and instance check artifacts. Press k for usage, retention and cleanup; b for build caches.\nPreferences: saved per project and instance.\n\nMouse: {}\nRun mode: {}\nPlanner approval: {}\nDelivery: {} via {}/{}\n\nDefaults apply to new tasks; existing revisions stay pinned.\nu User defaults: what new projects start with; preview and apply them here.\nf Profiles: named role models to reuse; select one here or as the new-project default.\nw Work categories: templates that tell AI turns how to execute, review, validate and remediate code, documents or art; override per project.\nChecks: list, add, edit, enable or disable.\nKeyboard remains available with mouse disabled.",
            screen.project_name,
            if screen.mouse_enabled {
                "Enabled"
            } else {
                "Disabled"
            },
            if screen.auto_advance {
                "Continue queue"
            } else {
                "Run one"
            },
            if self.review_required {
                "Review required"
            } else {
                "Automatic"
            },
            delivery.mode,
            delivery.remote,
            delivery.base_branch
        )
    }

    fn config_controls(&self) -> Vec<(&'static str, Action)> {
        vec![
            ("m Defaults", Action::ProjectModels),
            ("u User defaults", Action::UserDefaults),
            ("f Profiles", Action::Profiles),
            ("w Work categories", Action::Categories),
            ("c Checks", Action::ProjectChecks),
            ("g Git delivery", Action::Delivery),
            ("k Storage", Action::Storage),
            ("b Build cache", Action::BuildCache),
            ("e Preferences", Action::Configure),
            ("Esc Back", Action::Back),
        ]
    }

    fn status_progress(&self, task: &Task) -> Vec<String> {
        vec![if task.attempts.is_empty() {
            "No attempts yet".to_owned()
        } else {
            format!("Attempt {}", task.attempts.len())
        }]
    }

    fn status_step(&self, task: &Task) -> Option<String> {
        (task.status() == Status::Running).then(|| {
            format!(
                "Step: {}",
                model::running_step(task).map_or_else(|| model::active_phase(task), str::to_owned)
            )
        })
    }

    fn status_meter(&self, task: &Task, width: usize, compact: bool) -> Vec<String> {
        let attempt = task.attempts.last();
        // An attempt runs on the revision it started with.
        let settings = &attempt
            .map_or_else(|| task.definition(), |a| &a.revision)
            .settings;
        let model = &settings.models[model::active_role(task)];
        let mut usage = attempt.map_or_else(Vec::new, |attempt| {
            let tokens = if attempt.usage_tokens.chars().count() <= width {
                attempt.usage_tokens.clone()
            } else if attempt.usage_tokens_short.chars().count() <= width {
                attempt.usage_tokens_short.clone()
            } else if attempt.usage_tokens.contains('?') {
                "Tokens ? · Enter: details".to_owned()
            } else {
                "Tokens · Enter: details".to_owned()
            };
            vec![attempt_time(attempt), tokens, attempt.usage_cost.clone()]
        });
        let mut lines = Vec::new();
        if compact && usage.len() == 3 {
            lines.push(usage.remove(1));
        }
        lines.push(format!(
            "Model {}",
            model_label(&model.name, model.effort.as_deref())
        ));
        lines.extend(usage);
        lines.push(budget(task.used(), settings.attempt_limit));
        lines
    }

    fn running_situation(&self, task: &Task) -> String {
        model::active_phase(task)
    }

    fn ready_situation(&self, auto_advance: bool) -> &'static str {
        if auto_advance {
            "Ready · r Run queue; checks and repair run automatically"
        } else {
            "Run one selected · m Run mode to continue the queue"
        }
    }

    fn idle_output(&self) -> &'static str {
        "Ready. r starts an attempt in the background supervisor; this pane follows its log.\nDefaults and checks live in Config; c pins models for this task."
    }

    fn activity_title(&self, task: usize) -> String {
        format!(" Activity · #{task} · l: all ")
    }

    fn history_scope(&self) -> &'static str {
        "closed tasks"
    }

    fn history_columns(&self) -> &'static [Column] {
        &[
            Column {
                name: "Finished",
                left: true,
            },
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
            finished_label(task.closed_at_ms, domain::now_ms()),
            task.attempts.len().to_string(),
            duration(task.elapsed_ms()),
            self.usage(task.id).history_label(),
        ]
    }

    fn task_cost(&self, task: &Task) -> String {
        self.usage(task.id).cost_label()
    }

    fn attempt_lines(&self, task: &Task) -> String {
        task.attempts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                format!(
                    "\nAttempt {} · {} · {}\n{}",
                    i + 1,
                    a.state.label(),
                    attempt_time(a),
                    a.usage
                )
            })
            .collect()
    }
}

fn versioned(task: &domain::Task) -> VersionedTask {
    VersionedTask {
        id: task.id,
        version: task.version,
    }
}

/// The revision a Models form edits: a draft newer than the selected one, else the selected.
fn models_revision(task: &domain::Task) -> &domain::Revision {
    task.revisions
        .iter()
        .rev()
        .find(|r| r.lifecycle == "draft" && r.number > task.definition().number)
        .unwrap_or_else(|| task.definition())
}

fn breakdown_review(
    target: VersionedTask,
    proposal: crate::runner::agent::BreakdownProposal,
) -> Dialog {
    let body = format!(
        "{} subtask(s) before original:\n{}\nReason: {}\n\nApply supersedes the original; attempts stay in History. Approval follows planner preference. Cancel changes nothing.",
        proposal.tasks.len(),
        proposal
            .tasks
            .iter()
            .map(|t| format!(
                "• {}\n  {}\n  Criteria: {}",
                t.title,
                t.goal,
                t.criteria.join("; ")
            ))
            .collect::<Vec<_>>()
            .join("\n"),
        proposal.reason,
    );
    let mut dialog = Dialog::new(
        DialogKind::BreakdownReview { proposal },
        "Breakdown proposal · replace task",
        &body,
        vec![],
    );
    dialog.expected = Some(target);
    dialog
}

/// "#3 stopped · back to Pending", naming the stash that holds the attempt's changes.
fn control_notice(id: i64, action: Action, stash: Option<String>) -> String {
    let done = forms::control_done(action);
    match stash {
        Some(stash) => format!("#{id} {done} · changes stashed as \"{stash}\" (git stash list)"),
        None => format!("#{id} {done}"),
    }
}

fn defaults_preview(defaults: &[Option<ModelConfig>; 4]) -> String {
    ROLES
        .iter()
        .zip(defaults.iter())
        .map(|(role, value)| model::role_setting(role, value.as_ref()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn set_new_task_execution_model(
    definition: &mut Definition,
    identifier: &str,
    effort: &str,
) -> Result<(), String> {
    let identifier = identifier.trim();
    let effort = effort.trim();
    if !identifier.is_empty() {
        definition.models[0] = ModelConfig::from_fields(identifier, effort)?;
    } else if !effort.is_empty() {
        let model = definition.models[0]
            .as_mut()
            .ok_or("Set an execution provider/model before setting effort")?;
        model.effort = Some(effort.to_owned());
    }
    Ok(())
}

/// The category a saved task form names. Changing the kind while leaving the category
/// alone returns to the new kind's default; Human work has none.
fn chosen_category(
    chosen: &str,
    previous: &(domain::Kind, Option<String>),
    kind: domain::Kind,
) -> Result<Option<String>, String> {
    let explicit = (chosen != forms::DEFAULT_CATEGORY).then(|| chosen.to_owned());
    let unchanged = explicit.as_deref() == previous.1.as_deref();
    if kind == domain::Kind::Human {
        return match explicit {
            Some(_) if !unchanged => {
                Err("Human tasks are done by the owner and have no work category".into())
            }
            _ => Ok(None),
        };
    }
    Ok(if unchanged && kind != previous.0 {
        None
    } else {
        explicit
    })
}

fn before_id(text: &str) -> Result<Option<i64>, String> {
    if text.is_empty() {
        Ok(None)
    } else {
        text.trim_start_matches('#')
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| "Enter a task ID or leave blank for end of Queue".into())
    }
}
fn limit(text: &str) -> Result<Option<i64>, String> {
    if text.is_empty() {
        Ok(None)
    } else {
        text.parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| "Limits must be positive integers or blank".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_form_names_the_category_or_returns_to_the_kinds_default() {
        use domain::Kind::{Ai, Docs, Human};
        let named = |name: &str| Some(name.to_owned());
        // A new task: Default follows the kind, a choice is kept.
        assert_eq!(chosen_category("Default", &(Ai, None), Docs), Ok(None));
        assert_eq!(chosen_category("art", &(Ai, None), Ai), Ok(named("art")));
        // Editing: the kind changes while the category is left alone, so the new kind's
        // default applies; changing the category too keeps the choice.
        assert_eq!(
            chosen_category("code", &(Ai, named("code")), Docs),
            Ok(None)
        );
        assert_eq!(
            chosen_category("art", &(Ai, named("code")), Docs),
            Ok(named("art"))
        );
        assert_eq!(
            chosen_category("code", &(Ai, named("code")), Ai),
            Ok(named("code"))
        );
        // Human work has no category; choosing one is refused.
        assert_eq!(
            chosen_category("code", &(Ai, named("code")), Human),
            Ok(None)
        );
        assert_eq!(
            chosen_category("Default", &(Ai, named("code")), Human),
            Ok(None)
        );
        assert!(chosen_category("art", &(Ai, named("code")), Human).is_err());
    }

    #[test]
    fn new_task_execution_override_keeps_other_role_defaults() {
        let default = Some(ModelConfig {
            provider: "codex".into(),
            model: "default".into(),
            effort: Some("medium".into()),
        });
        let mut definition = Definition {
            title: "Dummy task".into(),
            goal: "Exercise model selection".into(),
            kind: domain::Kind::Ai,
            category: None,
            criteria: vec!["Saved".into()],
            models: std::array::from_fn(|_| default.clone()),
            attempt_limit: Some(3),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        set_new_task_execution_model(&mut definition, "dummy/worker", "high").unwrap();
        assert_eq!(
            definition.models[0],
            Some(ModelConfig {
                provider: "dummy".into(),
                model: "worker".into(),
                effort: Some("high".into()),
            })
        );
        assert_eq!(
            definition.models[1..],
            [default.clone(), default.clone(), default]
        );
        assert!(set_new_task_execution_model(&mut definition, "worker", "high").is_err());
    }

    #[test]
    fn whitespace_execution_fields_keep_configured_default_and_trim_effort() {
        let default = Some(ModelConfig {
            provider: "dummy".into(),
            model: "default".into(),
            effort: Some("medium".into()),
        });
        let mut definition = Definition {
            title: "Dummy task".into(),
            goal: "Use configured model".into(),
            kind: domain::Kind::Ai,
            category: None,
            criteria: vec!["Saved".into()],
            models: std::array::from_fn(|_| default.clone()),
            attempt_limit: Some(3),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        set_new_task_execution_model(&mut definition, "  \t ", " \t ").unwrap();
        assert_eq!(definition.models[0], default);
        set_new_task_execution_model(&mut definition, " \t ", "  high \t").unwrap();
        assert_eq!(
            definition.models[0].as_ref().unwrap().effort.as_deref(),
            Some("high")
        );
    }
}

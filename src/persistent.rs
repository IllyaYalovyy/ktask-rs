//! The SQLite workspace: forms become application commands, then read projections reload.
//! No SQL, demo execution, or write-back of UI task objects lives here.
use crate::{
    app::{Action, EvidenceSection, Screen, View},
    application::{
        self as domain, Definition, EvidenceCursor, EvidenceSearch, ModelConfig, Service,
        VersionedTask, transfer,
    },
    backend::{Backend, EvidenceAt},
    dialog::{Dialog, DialogKind, Field},
    forms, lens_ui,
    presentation::{self as model, ROLES, Status, Task},
    project_view::{
        self, APPROVAL, EvidencePageAt, FormChoices, before_id, chosen_category, control_notice,
        versioned,
    },
};
use std::{collections::BTreeMap, path::Path, time::Duration, time::Instant};

/// How often the terminal looks for changes other processes stored.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

const PROJECT_DEFAULTS: &str = "Project defaults";
const APPLY_DEFAULTS: &str = "Apply current project defaults";

pub(crate) struct StoredWorkspace {
    pub(crate) service: Service,
    /// The stored tasks the screen's projection came from, by id.
    pub(crate) stored: BTreeMap<usize, domain::Task>,
    proposal_job: Option<ProposalJob>,
    pub(crate) provision_job: Option<ProvisionJob>,
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

pub(crate) struct ProvisionJob {
    pub(crate) cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) handle: Option<std::thread::JoinHandle<Result<i64, String>>>,
    pub(crate) waiting_task: Option<crate::application::provision::ProvisionTarget>,
}

impl Drop for ProvisionJob {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
            provision_job: None,
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
        if let Err(error) = self.reload(screen) {
            screen.readiness = Some(crate::readiness::Readiness {
                lines: vec![format!(
                    "BLOCK · Refresh failed: {error} → Refresh before running"
                )],
                blockers: 1,
                refresh_failed: true,
            });
        }
        screen.dialog = Some(project_view::readiness_dialog(
            screen,
            self.launch_issue.as_deref(),
        ));
    }

    pub(crate) fn reload(&mut self, screen: &mut Screen) -> Result<(), String> {
        let data = project_view::read(&mut self.service, true)?;
        let alive = data.supervisor_alive;
        self.stored = project_view::show(screen, data)?;
        let current = screen.current().map(|task| task.id);
        self.launch_issue = project_view::launch_issue(&self.service, current.map(|id| id as i64))?;
        // While the supervisor runs the current attempt, nothing readiness checks can change,
        // and inspecting runs Git: inspect again once that run ends or another task is current.
        let running = project_view::running(screen);
        if running.is_none() || running != self.readiness_running {
            screen.readiness = Some(project_view::inspect_readiness(
                &self.service,
                current.and_then(|id| self.stored.get(&id)),
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

    pub(crate) fn committed(&mut self, screen: &mut Screen, message: &str) {
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
                if action == Action::Restart {
                    self.run(screen);
                } else if action == Action::MarkDone {
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

    fn poll_provision(&mut self, screen: &mut Screen) -> bool {
        if !self
            .provision_job
            .as_ref()
            .and_then(|job| job.handle.as_ref())
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            return false;
        }
        let mut job = self
            .provision_job
            .take()
            .expect("finished provisioning job");
        let result = job.handle.take().expect("provisioning handle").join();
        let message = match result {
            Ok(Ok(id)) => {
                match screen
                    .instance
                    .paths()
                    .map_err(|e| e.to_string())
                    .and_then(|paths| {
                        crate::runner::continue_after_provision(
                            &mut self.service,
                            &paths,
                            job.waiting_task.clone(),
                        )
                        .map_err(|e| e.to_string())
                    }) {
                    Ok((rechecked, resumed)) => format!(
                        "Android SDK generation #{id} published; dependency rechecked: {rechecked}; resumed: {resumed}"
                    ),
                    Err(error) => format!(
                        "Android SDK generation #{id} published; dependency recheck needs attention: {error}"
                    ),
                }
            }
            Ok(Err(error)) => format!("Android provisioning stopped: {error}"),
            Err(_) => "Android provisioning panicked; inspect progress and retry explicitly".into(),
        };
        if let Err(error) = self.reload(screen) {
            screen.message(error);
        } else {
            screen.message(message);
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
                    // The guardian can still hold the worker lease when the supervisor
                    // first disappears. Retry orphan settlement after it releases the
                    // lease even though liveness itself stays unchanged.
                    let running = screen
                        .current()
                        .is_some_and(|task| task.status() == Status::Running);
                    let unchanged =
                        crate::runner::supervisor_alive(&self.service).is_ok_and(|alive| {
                            Some(alive) == screen.supervisor_alive && (alive || !running)
                        });
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

    pub(crate) fn run(&mut self, screen: &mut Screen) {
        if screen
            .current()
            .is_some_and(|task| task.status() == Status::NeedsDependency)
        {
            let result = screen
                .instance
                .paths()
                .map_err(|e| e.to_string())
                .and_then(|paths| {
                    let id = screen.current().expect("current dependency").id;
                    let target = self.target(id)?;
                    self.service
                        .recheck_dependencies(
                            target,
                            &paths.cache.join("workers"),
                            &paths.executable,
                            &paths,
                        )
                        .map_err(|e| e.to_string())
                });
            match result {
                Ok(true) => self.committed(
                    screen,
                    "Dependencies verified · r Resume continues the task",
                ),
                Ok(false) => self.committed(
                    screen,
                    "Dependency not verified · u Readiness shows the observed result",
                ),
                Err(error) => screen.message(error),
            }
            return;
        }
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
                if self
                    .service
                    .current_task()
                    .ok()
                    .flatten()
                    .is_some_and(|task| {
                        task.state() == crate::application::lifecycle::State::NeedsDependency
                    })
                {
                    self.committed(
                        screen,
                        "Dependency not verified · u Readiness shows the observed result",
                    );
                    return;
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
                    // A refused repeat of a resource pause explains itself and offers the raise.
                    if let Some(at) = error.find("Resume refused")
                        && let Ok(dialog) = self.raise_dialog(id as usize, Some(&error[at..]))
                    {
                        screen.dialog = Some(dialog);
                    }
                }
                screen.message(error);
            }
        }
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

    fn answer_form(&self, screen: &Screen) -> Result<Option<Dialog>, String> {
        let Some(id) = screen.current().map(|t| t.id) else {
            return Ok(None);
        };
        match self
            .service
            .open_question(id as i64)
            .map_err(|e| e.to_string())?
        {
            Some((_, question)) => Ok(Some(project_view::with_version(
                forms::answer(id, &question),
                &self.stored,
                id,
            ))),
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
        screen: &mut Screen,
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
            if action == Action::Restart {
                self.service
                    .require_restart(target)
                    .map_err(|e| e.to_string())?;
            }
            self.service
                .request_control(
                    target,
                    if matches!(action, Action::Stop | Action::Restart) {
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
                if matches!(action, Action::Stop | Action::Restart) {
                    "Stop"
                } else {
                    "Mark done"
                }
            ));
        } else {
            let saved = if action == Action::Restart {
                crate::runner::control::restart(&mut self.service, target, &reason)
            } else if action == Action::Stop {
                crate::runner::control::stop(&mut self.service, target, &reason)
            } else {
                crate::runner::control::mark_done(&mut self.service, target, &reason)
            }
            .map_err(|e| e.to_string())?;
            self.saved_notice = Some(control_notice(target.id, action, saved.stash));
            if action == Action::Restart {
                self.committed(screen, "Restart prepared · starting from current HEAD");
                self.run(screen);
                self.saved_notice = None;
            }
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
                lenses: None,
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
        definition.kind = project_view::kind(dialog.get(forms::KIND));
        definition.category =
            chosen_category(dialog.get(forms::CATEGORY), &previous, definition.kind)?;
        definition.lenses = lens_ui::selection(dialog)?;
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
            DialogKind::Stop(id) | DialogKind::Restart(id) | DialogKind::MarkDone(id) => {
                let action = match dialog.kind {
                    DialogKind::Stop(_) => Action::Stop,
                    DialogKind::Restart(_) => Action::Restart,
                    _ => Action::MarkDone,
                };
                self.control(screen, dialog, action, id, target()?)?
            }
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
            // The application applies these: they act on projects, not on this one's tasks.
            | DialogKind::RegisterProject
            | DialogKind::MoveProject
            | DialogKind::DeleteProject
            | DialogKind::Reports
            | DialogKind::Report { .. }
            | DialogKind::ReportLimits(_)
            | DialogKind::Fixes
            | DialogKind::FixConfirm(_)
            | DialogKind::FixView { .. }
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
            | DialogKind::Lenses(_)
            | DialogKind::Lens { .. }
            | DialogKind::LensDefaults(_)
            | DialogKind::ProjectChecks(_)
            | DialogKind::ProjectCheck { .. }
            | DialogKind::Preferences(_)
            | DialogKind::Delivery(_)
            | DialogKind::Limits(_)
            | DialogKind::Limit(_)
            | DialogKind::RaiseLimit(_)
            | DialogKind::Storage(_)
            | DialogKind::StorageConfirm(_)
            | DialogKind::BuildCache(_)
            | DialogKind::BuildCacheConfirm(_)
            | DialogKind::Tools
            | DialogKind::ToolPreview
            | DialogKind::SettingsExport
            | DialogKind::SettingsOverwrite { .. }
            | DialogKind::SettingsImport
            | DialogKind::SettingsPreview { .. }
            | DialogKind::DemoSettingsPreview(_) => {
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
            section,
            page,
            chunk,
            artifact,
            step,
            match_sequence,
            watermark,
            ..
        } = dialog.kind
        else {
            return String::new();
        };
        let at = EvidencePageAt {
            attempt: record.id,
            section: EvidenceSection::from_index(section),
            page,
            chunk,
            artifact,
            step,
            selected_match: match_sequence.and_then(|sequence| {
                dialog
                    .fields
                    .first()
                    .map(|field| (sequence, field.value.as_str()))
            }),
            watermark,
        };
        let evidence = match screen.instance.paths() {
            Ok(paths) => project_view::evidence(&self.service, &paths.state, &at),
            Err(error) => format!("Evidence unavailable: {error}"),
        };
        project_view::inspect_attempt(dialog, task, record, &evidence)
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
            | Action::Lenses
            | Action::ProjectChecks
            | Action::Delivery
            | Action::Storage
            | Action::BuildCache
            | Action::Tools => Some(self.open_project_config(screen, action)?),
            Action::ResourceLimits => Some(self.limits_dialog()?),
            Action::RaiseResume => target
                .map(|task| self.raise_dialog(task.id, None))
                .transpose()?,
            Action::ExportSettings | Action::ImportSettings => Some(self.open_settings(action)?),
            Action::Breakdown => Some(self.propose_breakdown(screen)?),
            Action::Add | Action::Insert | Action::Edit => Some(project_view::task_form(
                screen,
                action,
                &self.stored,
                &FormChoices::read(&self.service),
                "in SQLite",
            )),
            Action::Move => target.map(|task| {
                project_view::with_version(forms::move_task(task.id), &self.stored, task.id)
            }),
            Action::Reject => target.map(|task| {
                project_view::with_version(forms::reject(task.id), &self.stored, task.id)
            }),
            Action::Models => target.map(|task| self.models_form(task)).transpose()?,
            Action::Stop
            | Action::Restart
            | Action::MarkDone
            | Action::CancelTask
            | Action::Supersede
            | Action::CopyToQueue => target.and_then(|task| {
                forms::control(action, task)
                    .map(|dialog| project_view::with_version(dialog, &self.stored, task.id))
            }),
            Action::Answer => self.answer_form(screen)?,
            Action::Help => Some(forms::help(&format!(
                "Changes save immediately. {}",
                project_view::HELP
            ))),
            Action::Import => Some(Dialog::new(
                DialogKind::Import,
                "Import task definitions · disk",
                "Read a ktask-tasks-v4 (or v3, v2, v1) TSV export. Preview before committing.\nImports create new tasks, not copied attempt/history records.",
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
                | DialogKind::Tools
                | DialogKind::ToolPreview
        ) {
            return self.save_project_config(screen, dialog);
        }
        if matches!(dialog.kind, DialogKind::Limits(_) | DialogKind::Limit(_)) {
            return self.save_limits_form(screen, dialog);
        }
        if let DialogKind::RaiseLimit(id) = dialog.kind {
            return self.raise_and_resume(screen, dialog, id);
        }
        if matches!(
            dialog.kind,
            DialogKind::SettingsExport
                | DialogKind::SettingsOverwrite { .. }
                | DialogKind::SettingsImport
                | DialogKind::SettingsPreview { .. }
        ) {
            return self.save_settings(screen, dialog);
        }
        let readonly = dialog.read_only();
        let exporting = matches!(
            dialog.kind,
            DialogKind::Export | DialogKind::FileOverwrite { .. }
        );
        let result = self.execute_form(screen, dialog)?;
        if result
            && !readonly
            && !(matches!(dialog.kind, DialogKind::Restart(_)) && self.pending_control.is_none())
        {
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
        Ok(Dialog::new(
            DialogKind::Stats,
            "Project statistics · SQLite",
            &project_view::stats(&self.service, screen.queue.len(), screen.history.len())?,
            vec![],
        ))
    }

    fn tick(&mut self, screen: &mut Screen) -> bool {
        if self.poll_provision(screen) || self.poll_proposal(screen) || self.poll_control(screen) {
            return true;
        }
        if screen.dialog.is_some() || screen.last_tick.elapsed() < REFRESH_INTERVAL {
            return false;
        }
        let changed = self.refresh(screen);
        // A slow snapshot must not make every queued input trigger another
        // snapshot. Leave an input-processing interval after refresh finishes.
        screen.last_tick = Instant::now();
        changed
    }

    fn busy(&self) -> bool {
        self.provision_job.is_some() || self.pending_control.is_some()
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
            None => project_view::inspect_overview(task),
        }
    }

    fn find_label(&self, section: EvidenceSection) -> Option<&'static str> {
        project_view::find_label(section)
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

    fn stored_task(&self, id: usize) -> Option<&domain::Task> {
        self.stored.get(&id)
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
        let warnings: String = self
            .service
            .limit_warnings()
            .unwrap_or_default()
            .iter()
            .map(|warning| format!("Limit warning: {warning}.\n"))
            .collect();
        format!(
            "Project: {}\n{warnings}Storage: SQLite output and instance check artifacts. k: usage and cleanup · b: build caches · R: resource limits.\nPreferences: saved per project and instance.\n\nMouse: {}\nRun mode: {}\nPlanner approval: {}\nDelivery: {} via {}/{}\nDependencies: manual external setup; Current Task and Configuration offer r Recheck when a prerequisite is missing. CLI require declares prerequisites before a task starts.\n\nDefaults apply to new tasks; existing revisions stay pinned.\nu User defaults: what new projects start with; preview and apply them here.\nf Profiles: named role models to reuse; select one here or as the new-project default.\nw Work categories: templates that tell AI turns how to execute, review, validate and remediate code, documents or art; override per project.\nL Review lenses: opt-in specialist reviews (concurrency, security, simplicity or your own) and the project's default selection.\nChecks: list, add, edit, enable or disable.\nKeyboard remains available with mouse disabled.\ni Report limits: time and cost of agent reports and fixes; their model is the operator role in m Defaults.\nE Export settings and o Import settings move this project's settings to another computer.",
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
            ("r Recheck current", Action::Run),
            ("m Defaults", Action::ProjectModels),
            ("u User defaults", Action::UserDefaults),
            ("f Profiles", Action::Profiles),
            ("w Work categories", Action::Categories),
            ("L Review lenses", Action::Lenses),
            ("c Checks", Action::ProjectChecks),
            ("g Git delivery", Action::Delivery),
            ("k Storage", Action::Storage),
            ("R Limits", Action::ResourceLimits),
            ("b Build cache", Action::BuildCache),
            ("t Tools", Action::Tools),
            ("i Report limits", Action::ReportLimits),
            ("E Export settings", Action::ExportSettings),
            ("o Import settings", Action::ImportSettings),
            ("e Preferences", Action::Configure),
            ("Esc Back", Action::Back),
        ]
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
            lenses: None,
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
            lenses: None,
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

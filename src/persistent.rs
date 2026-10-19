//! Terminal adapter: translate forms into application commands, then reload read projections.
//! No SQL, demo execution, or write-back of UI task objects lives here.
use crate::{
    app::{Action, App, Catalog, View},
    application::{self as domain, Definition, ModelConfig, Service, VersionedTask, transfer},
    dialog::{Dialog, DialogKind, Field},
    instance::Instance,
    presentation::{self as model, Kind, RevisionState, Status},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(crate) struct ProposalJob {
    target: VersionedTask,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<ProposalResult, String>>>,
}
pub(crate) enum ProposalResult {
    Remediation(crate::runner::codex::RemediationProposal),
    Breakdown(crate::runner::codex::BreakdownProposal),
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

const ABANDON_TASK: &str = "Whole task";
const ABANDON_ATTEMPT: &str = "This attempt";

impl App {
    pub(crate) fn stored_cost(&self, id: usize) -> String {
        cost_label(self.stored.get(&id).and_then(|t| {
            t.attempts
                .iter()
                .filter_map(|a| a.cost_microusd)
                .reduce(|a, b| a + b)
        }))
    }
    pub fn persistent(instance: Instance, service: Service) -> Result<Self, String> {
        let mut app = Self::empty(instance);
        app.project_name.clone_from(&service.project_name);
        app.service = Some(service);
        app.queue.clear();
        app.history.clear();
        app.files.clear();
        app.events.clear();
        app.queue_catalog = Catalog::default();
        app.history_catalog = Catalog::default();
        app.last_workspace = None;
        app.load_preferences()?;
        app.reload()?;
        app.message(
            "Runs happen in a background supervisor; quitting this screen does not stop them",
        );
        Ok(app)
    }
    pub(crate) fn reload(&mut self) -> Result<(), String> {
        let snapshot = self
            .service
            .as_mut()
            .expect("SQLite mode")
            .snapshot()
            .map_err(|e| e.to_string())?;
        let mut queue = snapshot
            .queue
            .iter()
            .map(project_task)
            .collect::<Result<Vec<_>, _>>()?;
        let mut history = snapshot
            .history
            .iter()
            .map(project_task)
            .collect::<Result<Vec<_>, _>>()?;
        let previous = self.current().map(|t| t.id);
        self.stored = snapshot
            .queue
            .into_iter()
            .chain(snapshot.history)
            .map(|t| (t.id as usize, t))
            .collect();
        let alive = crate::runner::supervisor_alive(self.service.as_ref().unwrap())
            .map_err(|e| e.to_string())?;
        self.supervisor_alive = alive;
        if !alive {
            for task in queue.iter_mut().chain(history.iter_mut()) {
                let abandoned = match task.status() {
                    Status::Running => true,
                    Status::Paused | Status::Waiting | Status::Question => {
                        let attempt = self.stored.get(&(task.id)).and_then(|t| t.attempts.last());
                        match attempt {
                            Some(attempt) => !self
                                .service
                                .as_ref()
                                .unwrap()
                                .supervisor_released(attempt.id)
                                .map_err(|e| e.to_string())?,
                            None => false,
                        }
                    }
                    _ => false,
                };
                if abandoned {
                    task.stored_status = Some(Status::Abandoned);
                    if let Some(attempt) = task.attempts.last_mut() {
                        attempt.state = Status::Abandoned;
                    }
                }
            }
        }
        self.queue = queue;
        self.history = history;
        let current = self.current().map(|t| t.id as i64);
        self.task_events = snapshot
            .events
            .iter()
            .filter(|(task, _)| current.is_some() && (*task == current || task.is_none()))
            .map(|(_, message)| message.clone())
            .collect();
        self.events = snapshot
            .events
            .into_iter()
            .map(|(_, message)| message)
            .collect();
        if self.current().map(|t| t.id) != previous {
            self.follow = true;
            self.show_log = false;
            self.log_offset = 0;
        }
        self.reconcile();
        Ok(())
    }
    /// Abandon gives up on the task. A waiting or paused attempt may be dropped alone instead.
    fn open_abandon(&mut self, id: usize) {
        let Some(task) = self.task(id) else {
            return;
        };
        let live = !task.status().failed();
        let title = format!("Abandon #{id} · {}", task.definition().title);
        let mut fields = vec![Field::text("Reason", "")];
        if live {
            fields.push(Field::choice(
                "Scope",
                &[ABANDON_TASK, ABANDON_ATTEMPT],
                ABANDON_TASK,
            ));
        }
        let body = if live {
            "Whole task: it moves to History as cancelled; the attempt is interrupted.\nThis attempt: interrupted without spending budget; the task stays current for Run."
        } else {
            "Gives up on this task. It moves to History as cancelled; attempts and evidence stay."
        };
        let mut dialog = Dialog::new(DialogKind::Abandon(id), &title, body, fields);
        dialog.expected = self.target(id).ok();
        self.dialog = Some(dialog);
    }
    fn target(&self, id: usize) -> Result<VersionedTask, String> {
        self.stored
            .get(&id)
            .map(|t| VersionedTask {
                id: t.id,
                version: t.version,
            })
            .ok_or_else(|| "Task no longer exists in this view".into())
    }
    fn committed(&mut self, message: &str) {
        match self.reload() {
            Ok(()) => self.message(message),
            Err(error) => self.message(format!("Saved to SQLite, but refresh failed: {error}")),
        }
    }
    pub(crate) fn persistent_action(&mut self, action: Action) -> bool {
        if matches!(
            action,
            Action::Configure | Action::ProjectModels | Action::ProjectChecks
        ) {
            if let Err(error) = self.open_project_config(action) {
                self.message(error);
            }
            return true;
        }
        if !self.enabled(action) {
            return false;
        }
        if matches!(action, Action::Breakdown) {
            if let Err(error) = self.propose_breakdown() {
                self.message(error);
            }
            return true;
        }
        if matches!(action, Action::Diagnose) {
            if let Err(error) = self.propose_remediation() {
                self.message(error);
            }
            return true;
        }
        if matches!(action, Action::Recover) {
            let id = self.current().expect("enabled current").id;
            let mut dialog = Dialog::new(
                DialogKind::Recover,
                "Recover abandoned attempt",
                "The supervisor is absent. Inspect the workspace and any worker descendants before recovery.\nIf a worker died without cleanup confirmation, explicitly confirm its cleanup below. Recovery retains repository changes and evidence; retry explicitly.",
                vec![Field::choice(
                    "Worker cleanup",
                    &["Not confirmed", "I confirm cleanup"],
                    "Not confirmed",
                )],
            );
            dialog.expected = self.target(id).ok();
            self.dialog = Some(dialog);
            return true;
        }
        let result = match action {
            Action::Run | Action::Retry => self
                .instance
                .paths()
                .map_err(|e| e.to_string())
                .and_then(|paths| {
                    crate::runner::background(
                        self.service.as_mut().expect("SQLite mode"),
                        &paths,
                        self.auto_advance,
                    )
                    .map_err(|e| e.to_string())
                }),
            Action::Pause => {
                let id = self.current().expect("enabled current").id;
                self.target(id).and_then(|target| {
                    self.service
                        .as_mut()
                        .unwrap()
                        .request_pause(target)
                        .map_err(|e| e.to_string())
                })
            }
            Action::Answer => {
                let id = self.current().expect("enabled current").id;
                match self.service.as_ref().unwrap().open_question(id as i64) {
                    Ok(Some((_, question))) => {
                        let mut dialog = Dialog::new(
                            DialogKind::Answer(id),
                            "Answer AI question",
                            &question,
                            vec![Field::text("Answer", "")],
                        );
                        dialog.expected = self.target(id).ok();
                        self.dialog = Some(dialog);
                    }
                    Ok(None) => self.message("No open question; use Run/resume to continue"),
                    Err(error) => self.message(error.to_string()),
                }
                return true;
            }
            Action::Abandon => {
                let id = self.current().expect("enabled current").id;
                self.open_abandon(id);
                return true;
            }
            Action::Delete
                if self
                    .selected_task()
                    .is_some_and(|t| crate::actions::abandonable(t.status())) =>
            {
                let id = self.selected_task().expect("enabled task").id;
                self.open_abandon(id);
                return true;
            }
            Action::Approve | Action::Acknowledge => {
                let id = self.selected_task().expect("enabled task").id;
                self.target(id).and_then(|target| {
                    let service = self.service.as_mut().expect("SQLite mode");
                    if matches!(action, Action::Approve) {
                        service.decide(target, true, "Approved by owner")
                    } else {
                        service.acknowledge(target)
                    }
                    .map_err(|e| e.to_string())
                })
            }
            Action::Undo => {
                let target = self.durable_undo.expect("enabled undo");
                let result = self
                    .service
                    .as_mut()
                    .expect("SQLite mode")
                    .restore(target)
                    .map_err(|e| e.to_string());
                if result.is_ok() {
                    self.durable_undo = None;
                }
                result
            }
            Action::Stats => {
                let attempts: Vec<_> = self.stored.values().flat_map(|t| &t.attempts).collect();
                self.dialog = Some(Dialog::new(
                    DialogKind::Stats,
                    "Project statistics · SQLite",
                    &format!(
                        "Queue: {}\nHistory: {}\nAttempts: {}\nActive time: {} ms\nRecorded cost: {}\nMissing cost is unknown, not zero.\nHuman acknowledgments do not create AI attempts.",
                        self.queue.len(),
                        self.history.len(),
                        attempts.len(),
                        attempts.iter().map(|a| a.active_ms).sum::<i64>(),
                        cost_label(
                            attempts
                                .iter()
                                .filter_map(|a| a.cost_microusd)
                                .reduce(|a, b| a + b)
                        )
                    ),
                    vec![],
                ));
                return true;
            }
            _ => return false,
        };
        match result {
            Ok(()) => {
                let continue_queue = matches!(action, Action::Acknowledge)
                    || (matches!(action, Action::Approve)
                        && self.selected_task().is_some_and(|selected| {
                            self.current()
                                .is_some_and(|current| current.id == selected.id)
                        }));
                self.committed(match action {
                    Action::Run | Action::Retry => {
                        "Supervisor started · close TUI safely · Pause stops execution"
                    }
                    Action::Pause => "Pause requested · waiting for worker shutdown and cleanup",
                    _ => "Saved to SQLite",
                });
                if continue_queue {
                    self.continue_queue();
                }
            }
            Err(error) => self.message(error),
        }
        true
    }

    /// A committed intervention must not be presented as a failed form if starting fails.
    pub(crate) fn continue_queue(&mut self) {
        if !self.auto_advance {
            self.continuation_pending = None;
            return;
        }
        self.continuation_pending = None;
        let runnable = self.current().is_some_and(|task| {
            task.definition().kind == Kind::Ai
                && (task.status() == Status::Pending
                    || task.status().failed()
                    || (task.status() == Status::Question
                        && self
                            .service
                            .as_ref()
                            .unwrap()
                            .open_question(task.id as i64)
                            .is_ok_and(|question| question.is_none())))
        });
        if !runnable {
            return;
        }
        if self.supervisor_alive {
            self.continuation_pending = self.current().map(|task| task.id);
            return;
        }
        let result = self
            .instance
            .paths()
            .map_err(|e| e.to_string())
            .and_then(|paths| {
                crate::runner::background(self.service.as_mut().unwrap(), &paths, true)
                    .map_err(|e| e.to_string())
            });
        match result {
            Ok(()) => self.committed("Saved to SQLite · queue continuing automatically"),
            Err(error) => self.message(format!("Saved to SQLite · queue stopped: {error}")),
        }
    }

    fn propose_remediation(&mut self) -> Result<(), String> {
        let id = self.current().ok_or("No current task")?.id;
        let task = self
            .service
            .as_mut()
            .unwrap()
            .task(id as i64)
            .map_err(|e| e.to_string())?
            .ok_or("Current task disappeared")?;
        let definition = task.definition().definition.clone();
        let model = definition.models[3]
            .clone()
            .ok_or("Configure a remediation model first")?;
        let paths = self.instance.paths().map_err(|e| e.to_string())?;
        let evidence = self
            .service
            .as_ref()
            .unwrap()
            .remediation_evidence(id as i64, &paths.state)
            .map_err(|e| e.to_string())?;
        let cwd = self
            .service
            .as_ref()
            .unwrap()
            .project_root()
            .map_err(|e| e.to_string())?;
        let target = self.target(id)?;
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancelled.clone();
        self.proposal_job = Some(ProposalJob {
            target,
            cancelled,
            handle: Some(std::thread::spawn(move || {
                crate::runner::codex::propose_remediation(
                    &stop,
                    &model,
                    &definition.title,
                    &definition.goal,
                    &evidence,
                    &paths.cache.join("workers"),
                    &cwd,
                )
                .map(ProposalResult::Remediation)
                .map_err(|e| e.to_string())
            })),
        });
        self.dialog = Some(Dialog::new(
            DialogKind::RemediationPending,
            "Requesting remediation proposal",
            "Reading failure evidence with the remediation model.\nCancel stops the request and changes nothing. Timeout: 120 seconds.",
            vec![],
        ));
        Ok(())
    }

    fn propose_breakdown(&mut self) -> Result<(), String> {
        let id = self.selected_task().ok_or("No selected task")?.id;
        let task = self
            .service
            .as_mut()
            .unwrap()
            .task(id as i64)
            .map_err(|e| e.to_string())?
            .ok_or("Task disappeared")?;
        let definition = task.definition().definition.clone();
        let model = self
            .service
            .as_ref()
            .unwrap()
            .project_defaults()
            .map_err(|e| e.to_string())?[0]
            .clone()
            .ok_or("Configure a planning model first")?;
        let paths = self.instance.paths().map_err(|e| e.to_string())?;
        let cwd = self
            .service
            .as_ref()
            .unwrap()
            .project_root()
            .map_err(|e| e.to_string())?;
        let target = self.target(id)?;
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancelled.clone();
        self.proposal_job = Some(ProposalJob {
            target,
            cancelled,
            handle: Some(std::thread::spawn(move || {
                crate::runner::codex::propose_breakdown(
                    &stop,
                    &model,
                    &definition.title,
                    &definition.goal,
                    &definition.criteria,
                    &paths.cache.join("workers"),
                    &cwd,
                )
                .map(ProposalResult::Breakdown)
                .map_err(|e| e.to_string())
            })),
        });
        self.dialog = Some(Dialog::new(
            DialogKind::BreakdownPending,
            "Requesting breakdown proposal",
            "Planning subtasks in execution order.\nCancel stops the request and changes nothing. Timeout: 120 seconds.",
            vec![],
        ));
        Ok(())
    }

    pub(crate) fn poll_remediation(&mut self) -> bool {
        let Some(job) = &self.proposal_job else {
            return false;
        };
        if !matches!(
            self.dialog.as_ref().map(|d| &d.kind),
            Some(DialogKind::RemediationPending | DialogKind::BreakdownPending)
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
            .unwrap_or_else(|_| Err("Remediation worker panicked".into()));
        self.dialog = None;
        match result {
            Ok(ProposalResult::Remediation(proposal)) => {
                self.show_remediation(job.target, proposal)
            }
            Ok(ProposalResult::Breakdown(proposal)) => self.show_breakdown(job.target, proposal),
            Err(error) => self.message(error),
        }
        true
    }

    fn show_breakdown(
        &mut self,
        target: VersionedTask,
        proposal: crate::runner::codex::BreakdownProposal,
    ) {
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
        self.dialog = Some(dialog);
    }

    fn show_remediation(
        &mut self,
        target: VersionedTask,
        proposal: crate::runner::codex::RemediationProposal,
    ) {
        let id = target.id as usize;
        let (title, body) = match &proposal {
            crate::runner::codex::RemediationProposal::Escalate { model, reason } => (
                "Remediation proposal · escalate model".to_owned(),
                format!(
                    "Execution model: {}/{} · {}\nReason: {reason}\n\nApply creates an approved revision; failed attempts remain unchanged. Cancel changes nothing.",
                    model.provider,
                    model.model,
                    model.effort.as_deref().unwrap_or("no effort")
                ),
            ),
            crate::runner::codex::RemediationProposal::Insert { tasks, reason } => (
                "Remediation proposal · insert repair tasks".to_owned(),
                format!(
                    "{} repair task(s) before current:\n{}\nReason: {reason}\n\nApproval follows planner preference. Cancel changes nothing.",
                    tasks.len(),
                    tasks
                        .iter()
                        .map(|t| format!(
                            "• {}\n  {}\n  Criteria: {}",
                            t.title,
                            t.goal,
                            t.criteria.join("; ")
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
        };
        let expected = Some(target);
        self.dialog = Some(Dialog::new(
            crate::dialog::DialogKind::RemediationReview { id, proposal },
            &title,
            &body,
            vec![],
        ));
        if let Some(dialog) = &mut self.dialog {
            dialog.expected = expected;
        }
    }

    /// Called after opening an existing form. Capture the revision version once, not at Save.
    pub(crate) fn prepare_persistent_dialog(&mut self) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let id = match dialog.kind {
            DialogKind::Task { id, .. } => id,
            DialogKind::Delete(id)
            | DialogKind::Move(id)
            | DialogKind::Models(id)
            | DialogKind::Resolve(id)
            | DialogKind::Abandon(id)
            | DialogKind::Reject(id) => Some(id),
            DialogKind::RemediationReview { id, .. } => Some(id),
            _ => None,
        };
        if dialog.expected.is_some() {
            return;
        }
        if let Some(task) = id.and_then(|id| self.stored.get(&id)) {
            dialog.expected = Some(VersionedTask {
                id: task.id,
                version: task.version,
            });
        }
        match dialog.kind {
            DialogKind::Task { id, .. } => {
                let ready = id
                    .and_then(|id| self.stored.get(&id))
                    .is_none_or(|t| t.definition().lifecycle == "ready");
                dialog.fields.push(Field::choice(
                    "Approval",
                    &["Ready", "Needs review"],
                    if ready { "Ready" } else { "Needs review" },
                ));
                dialog.body = "Save commits a new revision to SQLite. Earlier attempts remain unchanged.\nNeeds review creates a draft; any previous ready revision stays usable.".into();
            }
            DialogKind::Models(id) => {
                let task = &self.stored[&id];
                let revision = task
                    .revisions
                    .iter()
                    .rev()
                    .find(|r| r.lifecycle == "draft" && r.number > task.definition().number)
                    .unwrap_or_else(|| task.definition());
                for (i, model) in revision.definition.models.iter().enumerate() {
                    dialog.fields[i * 2] = Field::text(
                        &format!("{} provider/model", model::ROLES[i]),
                        &model
                            .as_ref()
                            .map_or(String::new(), |m| format!("{}/{}", m.provider, m.model)),
                    );
                    dialog.fields[i * 2 + 1] = Field::text(
                        &format!("{} effort (blank = none)", model::ROLES[i]),
                        model
                            .as_ref()
                            .and_then(|m| m.effort.as_deref())
                            .unwrap_or(""),
                    );
                }
                dialog.body = "Use provider/model identifiers; blank means unconfigured.\nSave approves a new revision. No provider calls are made.".into();
            }
            DialogKind::Config => {
                dialog.body = "Mouse preference applies to this session.\nTask models and limits are saved per revision. Codex runs in a separate supervisor.".into();
                dialog.fields.truncate(1);
            }
            DialogKind::Import => {
                dialog.title = "Import task definitions · disk".into();
                dialog.body = "Read a ktask-tasks-v1 TSV export. Preview before committing.\nImports create new tasks, not copied attempt/history records.".into();
                dialog.fields[0] = Field::text("Source", "tasks.tsv");
            }
            DialogKind::Export => {
                dialog.title = "Export task definitions · disk".into();
                dialog.body = "Versioned TSV includes criteria, provider/model/effort and limits.\nThis is a definition export, not a history backup.".into();
                dialog.fields[0] = Field::text("Destination", "tasks.tsv");
            }
            DialogKind::Inspect { id, .. } => {
                // The real read record has no simulated usage or answers.
                let task = &self.stored[&id];
                dialog.body = dialog.body.replace(" (simulated)", "");
                if let Some(start) = dialog.body.find("Total cost:")
                    && let Some(end) = dialog.body[start..].find('\n')
                {
                    dialog.body.replace_range(
                        start..start + end,
                        &format!(
                            "Recorded cost: {}",
                            cost_label(
                                task.attempts
                                    .iter()
                                    .filter_map(|a| a.cost_microusd)
                                    .reduce(|a, b| a + b)
                            )
                        ),
                    );
                }
                dialog
                    .body
                    .push_str("\nOutput is recorded execution evidence, not simulation.");
            }
            DialogKind::Help => {
                dialog.body = dialog.body.replace("All tasks, models, runs, and files are simulated.", "Changes save immediately. Run starts Codex in a background supervisor; Pause stops it.\nChecks, review and validation gate success. Failed work uses the remediation model.\nx abandons a waiting attempt without spending budget. Break down uses the planning model.");
            }
            _ => {}
        }
    }

    pub(crate) fn apply_persistent_dialog(&mut self, dialog: &mut Dialog) -> Result<bool, String> {
        if let DialogKind::BreakdownReview { proposal } = &dialog.kind {
            let target = dialog
                .expected
                .ok_or("Missing task version; reopen the form")?;
            let paths = self.instance.paths().map_err(|e| e.to_string())?;
            let ready = self
                .service
                .as_ref()
                .unwrap()
                .preferences(&paths.config)
                .map_err(|e| e.to_string())?
                .planner_approval
                == domain::preferences::PlannerApproval::Automatic;
            let defaults = self
                .service
                .as_ref()
                .unwrap()
                .defaults()
                .map_err(|e| e.to_string())?;
            let definitions = crate::runner::codex::breakdown_definitions(proposal, defaults);
            let ids = self
                .service
                .as_mut()
                .unwrap()
                .apply_breakdown(target, &definitions, &proposal.reason, ready)
                .map_err(|e| e.to_string())?;
            self.committed("Saved to SQLite");
            self.reveal(ids[0] as usize);
            return Ok(true);
        }
        if matches!(
            dialog.kind,
            DialogKind::ProjectModels(_)
                | DialogKind::ProjectChecks(_)
                | DialogKind::ProjectCheck { .. }
                | DialogKind::Preferences(_)
        ) {
            return self.save_project_config(dialog);
        }
        let readonly = dialog.read_only() || matches!(dialog.kind, DialogKind::Config);
        let exporting = matches!(
            dialog.kind,
            DialogKind::Export | DialogKind::FileOverwrite { .. }
        );
        let result = self.execute_form(dialog)?;
        if result && !readonly {
            self.committed(if exporting {
                "Exported to disk"
            } else {
                "Saved to SQLite"
            });
            if matches!(
                dialog.kind,
                DialogKind::Answer(_) | DialogKind::RemediationReview { .. }
            ) {
                self.continue_queue();
            }
        }
        Ok(result)
    }
    fn execute_form(&mut self, dialog: &mut Dialog) -> Result<bool, String> {
        let target = || {
            dialog
                .expected
                .ok_or_else(|| "Missing task version; reopen the form".to_owned())
        };
        match &dialog.kind {
            DialogKind::Recover => {
                crate::runner::run(
                    self.service.as_mut().unwrap(),
                    &self.instance.paths().map_err(|e| e.to_string())?,
                    &crate::runner::Options {
                        recover: true,
                        confirm_worker_cleanup: dialog.value(0) == "I confirm cleanup",
                        recover_target: Some(target()?),
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?;
            }
            DialogKind::RemediationPending | DialogKind::BreakdownPending => return Ok(false),
            DialogKind::BreakdownReview { .. } => unreachable!("handled before generic forms"),
            DialogKind::RemediationReview { id: _, proposal } => {
                let mut repairs = Vec::new();
                let model = match proposal {
                    crate::runner::codex::RemediationProposal::Escalate { model, .. } => {
                        Some(model.clone())
                    }
                    crate::runner::codex::RemediationProposal::Insert { tasks, .. } => {
                        let defaults = self
                            .service
                            .as_ref()
                            .unwrap()
                            .defaults()
                            .map_err(|e| e.to_string())?;
                        repairs = tasks
                            .iter()
                            .map(|t| Definition {
                                title: t.title.clone(),
                                goal: t.goal.clone(),
                                kind: domain::Kind::Ai,
                                criteria: t.criteria.clone(),
                                models: defaults.clone(),
                                attempt_limit: Some(3),
                                timeout_ms: Some(600_000),
                            })
                            .collect();
                        None
                    }
                };
                let reason = match proposal {
                    crate::runner::codex::RemediationProposal::Escalate { reason, .. }
                    | crate::runner::codex::RemediationProposal::Insert { reason, .. } => reason,
                };
                let ready = !self.review_required;
                self.service
                    .as_mut()
                    .unwrap()
                    .apply_remediation(target()?, model, &repairs, reason, ready)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Answer(_) => {
                self.service
                    .as_mut()
                    .unwrap()
                    .answer(target()?, dialog.value(0))
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Abandon(_) => {
                let service = self.service.as_mut().unwrap();
                let attempt_only = dialog.fields.len() > 1 && dialog.value(1) == ABANDON_ATTEMPT;
                if attempt_only {
                    crate::runner::abandon(service, target()?, dialog.value(0))
                } else {
                    crate::runner::abandon_task(service, target()?, dialog.value(0))
                }
                .map_err(|e| e.to_string())?;
            }
            DialogKind::Task { id, insert } => {
                let mut definition = if let Some(id) = id {
                    self.stored
                        .get(id)
                        .ok_or("Task no longer available")?
                        .definition()
                        .definition
                        .clone()
                } else {
                    Definition {
                        title: String::new(),
                        goal: String::new(),
                        kind: domain::Kind::Ai,
                        criteria: Vec::new(),
                        models: self
                            .service
                            .as_ref()
                            .unwrap()
                            .defaults()
                            .map_err(|e| e.to_string())?,
                        attempt_limit: Some(3),
                        timeout_ms: Some(600_000),
                    }
                };
                definition.title = dialog.value(0).into();
                definition.goal = dialog.value(1).into();
                if definition.criteria.join("\n") != dialog.value(2) {
                    definition.criteria = dialog.value(2).lines().map(str::to_owned).collect();
                }
                definition.kind = if dialog.value(3) == "Human" {
                    domain::Kind::Human
                } else {
                    domain::Kind::Ai
                };
                let ready = dialog.value(if *insert { 5 } else { 4 }) == "Ready";
                let selected = if let Some(id) = id {
                    self.service
                        .as_mut()
                        .unwrap()
                        .revise(target()?, &definition, ready)
                        .map_err(|e| e.to_string())?;
                    *id
                } else {
                    let before = if *insert {
                        before_id(dialog.value(4))?
                    } else {
                        None
                    };
                    self.service
                        .as_mut()
                        .unwrap()
                        .create(&[definition], before, ready)
                        .map_err(|e| e.to_string())?[0] as usize
                };
                self.queue_catalog.selected = Some(selected);
                if id.is_none() {
                    self.view = View::Queue;
                    self.queue_catalog.query.clear();
                }
            }
            DialogKind::Models(id) => {
                let task = self.stored.get(id).ok_or("Task no longer available")?;
                let revision = task
                    .revisions
                    .iter()
                    .rev()
                    .find(|r| r.lifecycle == "draft" && r.number > task.definition().number)
                    .unwrap_or_else(|| task.definition());
                let mut definition = revision.definition.clone();
                for (i, model) in definition.models.iter_mut().enumerate() {
                    let value = dialog.value(i * 2);
                    let effort = dialog.value(i * 2 + 1);
                    *model = if value.is_empty() {
                        if !effort.is_empty() {
                            return Err("Effort requires a provider/model".into());
                        }
                        None
                    } else {
                        let (provider, model) = value
                            .split_once('/')
                            .ok_or("Use provider/model, or leave blank")?;
                        Some(ModelConfig {
                            provider: provider.trim().into(),
                            model: model.trim().into(),
                            effort: (!effort.is_empty()).then(|| effort.to_owned()),
                        })
                    };
                }
                definition.attempt_limit = limit(dialog.value(8))?;
                definition.timeout_ms = limit(dialog.value(9))?;
                self.service
                    .as_mut()
                    .unwrap()
                    .revise(target()?, &definition, true)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Delete(_) => {
                let target = target()?;
                self.service
                    .as_mut()
                    .unwrap()
                    .delete(target)
                    .map_err(|e| e.to_string())?;
                self.durable_undo = Some(VersionedTask {
                    version: target.version + 1,
                    ..target
                });
            }
            DialogKind::Move(_) => self
                .service
                .as_mut()
                .unwrap()
                .move_before(target()?, before_id(dialog.value(0))?)
                .map_err(|e| e.to_string())?,
            DialogKind::Reject(_) => self
                .service
                .as_mut()
                .unwrap()
                .decide(target()?, false, dialog.value(0))
                .map_err(|e| e.to_string())?,
            DialogKind::Resolve(_) => {
                let outcome = match dialog.value(0) {
                    "Failed skipped" => domain::Resolution::FailedSkipped,
                    "Superseded" => domain::Resolution::Superseded,
                    _ => domain::Resolution::SucceededOverride,
                };
                self.service
                    .as_mut()
                    .unwrap()
                    .resolve(target()?, outcome, dialog.value(1))
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Config => {
                self.mouse_enabled = dialog.value(0) == "Enabled";
                self.message("Mouse preference applied · session only");
                return Ok(true);
            }
            DialogKind::Import => {
                let definitions =
                    transfer::import(Path::new(dialog.value(0))).map_err(|e| e.to_string())?;
                dialog.title = "Import preview · SQLite".into();
                dialog.body = definitions
                    .iter()
                    .map(|d| format!("{} · {}", d.kind.as_str(), d.title))
                    .collect::<Vec<_>>()
                    .join("\n");
                dialog.kind = DialogKind::StoredImport(definitions);
                dialog.fields = vec![Field::text("Before task ID", "")];
                dialog.focus = 0;
                dialog.error.clear();
                return Ok(false);
            }
            DialogKind::StoredImport(definitions) => {
                let ids = self
                    .service
                    .as_mut()
                    .unwrap()
                    .import(definitions, before_id(dialog.value(0))?)
                    .map_err(|e| e.to_string())?;
                self.queue_catalog.selected = Some(ids[0] as usize);
                self.queue_catalog.query.clear();
            }
            DialogKind::Export => {
                let tasks: Vec<_> = match dialog.value(1) {
                    "History" => self.history.iter().collect(),
                    "All" => self.queue.iter().chain(&self.history).collect(),
                    _ => self.queue.iter().collect(),
                };
                let definitions = tasks
                    .iter()
                    .map(|t| self.stored[&t.id].definition().definition.clone())
                    .collect::<Vec<_>>();
                let data = transfer::encode(&definitions).map_err(|e| e.to_string())?;
                let path = dialog.value(0).to_owned();
                if Path::new(&path).symlink_metadata().is_ok() {
                    let expected = transfer::read(Path::new(&path)).map_err(|e| e.to_string())?;
                    dialog.title = "Overwrite file?".into();
                    dialog.body =
                        format!("{path}\nConfirm replaces this file. Cancel keeps its contents.");
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
            }
            DialogKind::FileOverwrite {
                path,
                data,
                expected,
            } => transfer::export(Path::new(path), data, Some(expected))
                .map_err(|e| e.to_string())?,
            DialogKind::Help
            | DialogKind::Stats
            | DialogKind::Events
            | DialogKind::Inspect { .. } => return Ok(true),
            _ => return Err("Simulation is available only with --demo".into()),
        }
        Ok(true)
    }
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
fn cost_label(cost: Option<i64>) -> String {
    cost.map_or("not recorded".into(), |n| {
        format!("${:.6}", n as f64 / 1_000_000.0)
    })
}

fn status(value: &str) -> Result<Status, String> {
    Ok(match value {
        "new" => Status::New,
        "ready" => Status::Pending,
        "running" => Status::Running,
        "paused" => Status::Paused,
        "waiting" => Status::Waiting,
        "failed" => Status::Failed,
        "succeeded" => Status::Done,
        "succeeded_override" => Status::Override,
        "failed_skipped" => Status::Skipped,
        "superseded" => Status::Superseded,
        "cancelled" => Status::Cancelled,
        "blocked" => Status::Blocked,
        "interrupted" => Status::Interrupted,
        _ => return Err(format!("Unsupported stored status: {value}")),
    })
}
fn project_revision(revision: &domain::Revision) -> Result<model::Revision, String> {
    let d = &revision.definition;
    Ok(model::Revision {
        number: revision.number as usize,
        state: match revision.lifecycle.as_str() {
            "ready" => RevisionState::Ready,
            "draft" => RevisionState::Draft,
            "rejected" => RevisionState::Rejected,
            "retired" => RevisionState::Retired,
            _ => return Err("Unsupported revision lifecycle".into()),
        },
        title: d.title.clone(),
        goal: d.goal.clone(),
        check: d.criteria.join("\n"),
        kind: if d.kind == domain::Kind::Ai {
            Kind::Ai
        } else {
            Kind::Human
        },
        settings: model::Settings {
            models: std::array::from_fn(|i| model::Model {
                name: d.models[i].as_ref().map_or("unconfigured".into(), |m| {
                    format!("{}/{}", m.provider, m.model)
                }),
                effort: d.models[i].as_ref().and_then(|m| m.effort.clone()),
            }),
            attempt_limit: d.attempt_limit.unwrap_or(0) as usize,
            timeout_ms: d.timeout_ms.unwrap_or(0) as u64,
        },
        actor: revision
            .actor
            .clone()
            .unwrap_or_else(|| "Needs approval".into()),
        repaired: false,
    })
}
fn step_label(role: &str) -> &'static str {
    match role {
        "execution" => "Execution",
        "check" => "Checks",
        "review" => "Review",
        "validation" => "Validation",
        _ => "Remediation",
    }
}
/// The attempt's own report first, then each step's finding; empty until something was recorded.
/// An open question is shown in its own panel, so it is not repeated here.
fn report_text(attempt: &domain::Attempt) -> String {
    let mut text = attempt
        .report
        .clone()
        .filter(|report| {
            attempt.question.as_deref() != Some(report.trim())
                && attempt.outcome.as_deref().map(str::trim) != Some(report.trim())
        })
        .unwrap_or_default();
    let last = attempt.steps.len();
    for (i, step) in attempt.steps.iter().enumerate() {
        let finding = step.report.as_deref().unwrap_or_default().trim();
        if finding.is_empty() || text.contains(finding) {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let state = if attempt.state == "waiting" && i + 1 == last {
            "waiting"
        } else {
            step.state.as_str()
        };
        text.push_str(&format!("{} · {state}: {finding}", step_label(&step.role)));
    }
    text
}
fn project_task(task: &domain::Task) -> Result<model::Task, String> {
    let revisions = task
        .revisions
        .iter()
        .map(project_revision)
        .collect::<Result<Vec<_>, _>>()?;
    let attempts = task
        .attempts
        .iter()
        .map(|a| {
            let revision = revisions
                .iter()
                .find(|r| r.number == a.revision as usize)
                .ok_or("Attempt references missing revision")?
                .clone();
            let state = if a.state == "waiting" && a.wait_reason.as_deref() == Some("question") {
                Status::Question
            } else {
                status(&a.state)?
            };
            Ok(model::Attempt {
                revision,
                session: a.session.map(|id| id as usize),
                state,
                resume_at: a.resume_at_ms.map(|at| {
                    Instant::now()
                        + Duration::from_millis(at.saturating_sub(domain::now_ms()).max(0) as u64)
                }),
                logs: a.logs.clone(),
                outcome: a.outcome.clone().unwrap_or_else(|| match a.state.as_str() {
                    "waiting" => "Waiting".to_owned(),
                    state => state.to_owned(),
                }),
                report: report_text(a),
                question: a.question.clone(),
                steps: a
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        // A waiting attempt resumes its last step; it has not failed.
                        let state = if a.state == "waiting" && i + 1 == a.steps.len() {
                            "waiting".to_owned()
                        } else {
                            s.state.clone()
                        };
                        (step_label(&s.role).to_owned(), state)
                    })
                    .collect(),
                step: 0,
                cost_cents: a.cost_microusd.unwrap_or(0) as usize / 10_000,
                elapsed_ms: a.active_ms as u64,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut stored_status = status(&task.status)?;
    if task.lifecycle == "cancelled" && !revisions.iter().any(|r| r.state == RevisionState::Ready) {
        stored_status = Status::Rejected;
    }
    if stored_status == Status::Waiting
        && attempts.last().is_some_and(|a| a.state == Status::Question)
    {
        stored_status = Status::Question;
    }
    Ok(model::Task {
        id: task.id as usize,
        stored_status: Some(stored_status),
        stored_budget: Some(task.attempts_used as usize),
        revisions,
        attempts,
        resolution: None,
        resolution_reason: task
            .resolution_reason
            .as_ref()
            .map_or(String::new(), |reason| {
                format!(
                    "{} · {reason}",
                    task.resolved_by.as_deref().unwrap_or("unknown actor")
                )
            }),
        answer: String::new(),
        needs_question: false,
        needs_rate_limit: false,
        repairs: None,
    })
}

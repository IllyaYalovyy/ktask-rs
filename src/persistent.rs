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
    Remediation(crate::runner::agent::Judgment),
    Breakdown(crate::runner::agent::BreakdownProposal),
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
    pub(crate) fn open_readiness(&mut self) {
        let report = match self.reload() {
            Ok(()) => self.readiness.clone().expect("reload inspects readiness"),
            Err(error) => crate::readiness::Readiness {
                lines: vec![format!(
                    "BLOCK · Refresh failed: {error} → Refresh before running"
                )],
                blockers: 1,
                refresh_failed: true,
                resume_blocked: true,
            },
        };
        self.readiness = Some(report.clone());
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
        if let Some(session) = &self.execution_session
            && !report.refresh_failed
            && session.state != "running"
            && session.state != "completed"
        {
            body.push_str(&format!(
                "\n\nQueue: {} · {}",
                session.state, session.reason
            ));
        }
        body.push_str("\n\nAfter fixing setup, Refresh here or Run/Retry from Current. Inspection creates no attempt.");
        self.dialog = Some(Dialog::new(
            DialogKind::Readiness,
            "Execution readiness",
            &body,
            vec![],
        ));
    }
    pub(crate) fn stored_cost(&self, id: usize) -> String {
        let Some(task) = self.stored.get(&id) else {
            return "unknown".into();
        };
        if task.attempts.is_empty() {
            return "unknown".into();
        }
        cost_label(task.attempts.iter().try_fold(0_i64, |sum, attempt| {
            sum.checked_add(attempt.usage.cost_microusd?)
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
        self.execution_session = self
            .service
            .as_ref()
            .unwrap()
            .execution_session()
            .map_err(|e| e.to_string())?;
        let snapshot = self
            .service
            .as_mut()
            .expect("SQLite mode")
            .snapshot_brief()
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
        self.launch_issue = match self.current().map(|task| task.id as i64) {
            Some(id) => self
                .service
                .as_ref()
                .unwrap()
                .latest_launch_issue(id)
                .map_err(|e| e.to_string())?,
            None => None,
        };
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
        self.readiness = Some(crate::readiness::inspect(self));
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
        if matches!(action, Action::Readiness) {
            self.open_readiness();
            return true;
        }
        if matches!(
            action,
            Action::Configure | Action::ProjectModels | Action::ProjectChecks | Action::Storage
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
                if self
                    .current()
                    .is_some_and(|task| task.status() == Status::Abandoned)
                {
                    "Recover abandoned attempt"
                } else {
                    "Recover workspace ownership"
                },
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
                    let resume = self.current().is_some_and(|task| {
                        matches!(
                            task.status(),
                            Status::Paused | Status::Waiting | Status::Question
                        )
                    });
                    let queue = if resume {
                        self.execution_session
                            .as_ref()
                            .map_or(self.auto_advance, |session| session.mode == "queue")
                    } else {
                        self.auto_advance
                    };
                    crate::runner::background(
                        self.service.as_mut().expect("SQLite mode"),
                        &paths,
                        queue,
                        false,
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
                let usage = match self.service.as_ref().unwrap().project_usage() {
                    Ok(usage) => usage,
                    Err(error) => {
                        self.message(error.to_string());
                        return true;
                    }
                };
                let timing = match self.service.as_ref().unwrap().project_timing() {
                    Ok(timing) => timing,
                    Err(error) => {
                        self.message(error.to_string());
                        return true;
                    }
                };
                self.dialog = Some(Dialog::new(
                    DialogKind::Stats,
                    "Project statistics · SQLite",
                    &format!(
                        "Queue: {}\nHistory: {}\nAttempts: {} ({} failed/interrupted)\nActive time: {} ms\nWaiting time: {} ms\nProject usage: {}\nHuman acknowledgments do not create AI attempts.",
                        self.queue.len(),
                        self.history.len(),
                        timing.attempts,
                        timing.failed,
                        timing.active_ms,
                        timing.waiting_ms,
                        usage.label()
                    ),
                    vec![],
                ));
                return true;
            }
            _ => return false,
        };
        match result {
            Ok(()) => {
                if matches!(action, Action::Run | Action::Retry)
                    && let Some(id) = self.current().map(|task| task.id as i64)
                {
                    let _ = self.service.as_mut().unwrap().record_launch_result(id, "");
                }
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
            Err(error) => {
                if matches!(action, Action::Run | Action::Retry)
                    && let Some(id) = self.current().map(|task| task.id as i64)
                {
                    let _ = self
                        .service
                        .as_mut()
                        .unwrap()
                        .record_launch_result(id, &error);
                    let _ = self.reload();
                }
                self.message(error);
            }
        }
        true
    }

    /// A committed intervention must not be presented as a failed form if starting fails.
    pub(crate) fn continue_queue(&mut self) {
        let result = self
            .instance
            .paths()
            .map_err(|e| e.to_string())
            .and_then(|paths| {
                crate::runner::continue_after_intervention(self.service.as_mut().unwrap(), &paths)
                    .map_err(|e| e.to_string())
            });
        match result {
            Ok(true) => self.committed("Saved to SQLite · queue continuing automatically"),
            Ok(false) => {
                if let Err(error) = self.reload() {
                    self.message(format!("Saved to SQLite, but refresh failed: {error}"));
                }
            }
            Err(error) => self.committed(&format!("Saved to SQLite · queue waiting: {error}")),
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
        let target = self.target(id)?;
        let pin = self
            .service
            .as_mut()
            .unwrap()
            .begin_remediation_evidence(target)
            .map_err(|e| e.to_string())?;
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
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancelled.clone();
        self.proposal_job = Some(ProposalJob {
            target,
            cancelled,
            handle: Some(std::thread::spawn(move || {
                crate::runner::agent::propose_remediation(
                    &stop,
                    &model,
                    &definition,
                    &evidence,
                    &paths.cache.join("workers"),
                    &cwd,
                    pin,
                )
                .map(ProposalResult::Remediation)
                .map_err(|e| e.to_string())
            })),
        });
        self.dialog = Some(Dialog::new(
            DialogKind::RemediationPending,
            "Requesting failure judgment",
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
                crate::runner::agent::propose_breakdown(
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
            Ok(ProposalResult::Remediation(judgment)) => {
                self.show_remediation(job.target, judgment)
            }
            Ok(ProposalResult::Breakdown(proposal)) => self.show_breakdown(job.target, proposal),
            Err(error) => self.message(error),
        }
        true
    }

    fn show_breakdown(
        &mut self,
        target: VersionedTask,
        proposal: crate::runner::agent::BreakdownProposal,
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
        judgment: crate::runner::agent::Judgment,
    ) {
        let id = target.id as usize;
        let (title, body) = match &judgment {
            crate::runner::agent::Judgment::Repair { plan, reason } => (
                "Failure judgment · repair plan".to_owned(),
                format!(
                    "Plan: {plan}\nReason: {reason}\n\nApply records this plan for the next failure judge. Cancel changes nothing."
                ),
            ),
            crate::runner::agent::Judgment::Revise {
                title,
                goal,
                criteria,
                reason,
            } => (
                "Failure judgment · revise task".to_owned(),
                format!(
                    "Title: {title}\nGoal: {goal}\nCriteria: {}\nReason: {reason}\n\nApply creates an AI judge revision. Cancel changes nothing.",
                    criteria.join("; ")
                ),
            ),
            crate::runner::agent::Judgment::Escalate { model, reason } => (
                "Failure judgment · escalate model".to_owned(),
                format!(
                    "Execution model: {}/{} · {}\nReason: {reason}\n\nApply creates an approved revision; failed attempts remain unchanged. Cancel changes nothing.",
                    model.provider,
                    model.model,
                    model.effort.as_deref().unwrap_or("no effort")
                ),
            ),
            crate::runner::agent::Judgment::Insert { tasks, reason } => (
                "Failure judgment · insert prerequisites".to_owned(),
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
            crate::runner::agent::Judgment::Ask { question, reason } => (
                "Failure judgment · ask owner".to_owned(),
                format!(
                    "Question: {question}\nReason: {reason}\n\nApply saves an answerable question on failed current. Cancel changes nothing."
                ),
            ),
        };
        let expected = Some(target);
        self.dialog = Some(Dialog::new(
            crate::dialog::DialogKind::RemediationReview { id, judgment },
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
                dialog.body = "Use provider/model: codex/…, claude/sonnet, opencode/vendor/model; blank means unconfigured.\nSave approves a new revision. No provider calls are made.".into();
            }
            DialogKind::Config => {
                dialog.body = "Mouse preference applies to this session.\nTask models and limits are saved per revision. Agents run in a separate supervisor.".into();
                dialog.fields.truncate(1);
            }
            DialogKind::Import => {
                dialog.title = "Import task definitions · disk".into();
                dialog.body = "Read a ktask-tasks-v2 (or v1) TSV export. Preview before committing.\nImports create new tasks, not copied attempt/history records.".into();
                dialog.fields[0] = Field::text("Source", "tasks.tsv");
            }
            DialogKind::Export => {
                dialog.title = "Export task definitions · disk".into();
                dialog.body = "Versioned TSV includes criteria, provider/model/effort and limits.\nThis is a definition export, not a history backup.".into();
                dialog.fields[0] = Field::text("Destination", "tasks.tsv");
            }
            DialogKind::Inspect {
                id,
                attempt,
                section,
                page,
                chunk,
                artifact,
                match_sequence,
                watermark,
                ref excerpt,
                ..
            } => {
                let task = &self.stored[&id];
                let overview = task_overview(task);
                if let Some(record) = task.attempts.get(attempt) {
                    let service = self.service.as_ref().unwrap();
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
                        _ => self.instance.paths().map_err(Into::into).and_then(|paths| {
                            service.browse_transcript(record.id, artifact, page, &paths.state)
                        }),
                    };
                    dialog.body = format!(
                        "Attempt {} / {} · #{} · revision {} · {}\nOutcome: {}\n{}{}{}",
                        attempt + 1,
                        task.attempts.len(),
                        record.id,
                        record.revision,
                        record.state,
                        record.outcome.as_deref().unwrap_or("No outcome recorded"),
                        record
                            .report
                            .as_deref()
                            .filter(|report| Some(*report) != record.outcome.as_deref())
                            .map_or(String::new(), |report| format!("{report}\n")),
                        if excerpt.is_empty() {
                            String::new()
                        } else {
                            format!("Match: {excerpt}\n")
                        },
                        evidence.unwrap_or_else(|error| format!("Evidence unavailable: {error}"))
                    );
                    let revision = task
                        .revisions
                        .iter()
                        .find(|revision| revision.number == record.revision)
                        .unwrap_or_else(|| task.definition());
                    dialog.body.push_str(&format!(
                        "\n\nAttempt inputs: revision {} · session {}\n{}\nActive {} ms · waiting {} ms\nUsage: {}\n{}\n\n{overview}",
                        record.revision,
                        record
                            .session
                            .map_or("not recorded".into(), |id| id.to_string()),
                        revision_overview(revision),
                        record.active_ms,
                        record.waiting_ms,
                        record.usage.label(),
                        record.role_usage.iter().map(|role| format!("{} {} / {}: {} · provider elapsed {} ms", role.role, role.model, role.effort.as_deref().unwrap_or("none"), role.totals.label(), role.provider_elapsed_ms)).collect::<Vec<_>>().join("\n"),
                    ));
                } else {
                    dialog.body = format!("{overview}\nNo attempt evidence recorded.");
                }
            }
            DialogKind::Help => {
                dialog.body = dialog.body.replace("All tasks, models, runs, and files are simulated.", "Changes save immediately. Run starts the configured agents in a background supervisor; Pause stops it.\nChecks, review and validation gate success. Review findings enter the normal fix loop, with up to ten repair rounds before critical findings need a decision.\nx abandons a waiting attempt without spending budget. Break down uses the planning model.");
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
            let definitions = crate::runner::agent::breakdown_definitions(proposal, defaults);
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
                | DialogKind::Storage(_)
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
            let continue_after_remediation = match &dialog.kind {
                DialogKind::RemediationReview {
                    judgment: crate::runner::agent::Judgment::Ask { .. },
                    ..
                } => false,
                DialogKind::RemediationReview {
                    judgment: crate::runner::agent::Judgment::Revise { .. },
                    ..
                } => !self.review_required,
                DialogKind::RemediationReview { .. } => true,
                _ => false,
            };
            if matches!(dialog.kind, DialogKind::Answer(_)) || continue_after_remediation {
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
            DialogKind::RemediationReview { id: _, judgment } => {
                let ready = !self.review_required;
                crate::runner::remediation::apply_judgment(
                    self.service.as_mut().unwrap(),
                    target()?,
                    judgment,
                    ready,
                )
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
                        timeout_ms: None,
                        stage_timeout_ms: None,
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
                    *model =
                        ModelConfig::from_fields(dialog.value(i * 2), dialog.value(i * 2 + 1))?;
                }
                definition.attempt_limit = limit(dialog.value(8))?;
                definition.timeout_ms = crate::application::limits::parse(dialog.value(9))?;
                definition.stage_timeout_ms = crate::application::limits::parse(dialog.value(10))?;
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
    cost.map_or("unknown".into(), |n| {
        format!("${:.6}", n as f64 / 1_000_000.0)
    })
}
fn number(value: Option<i64>) -> String {
    value.map_or("?".into(), |n| n.to_string())
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
            stage_timeout_ms: d.stage_timeout_ms.unwrap_or(0) as u64,
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
        if let Some(counts) = step.counts {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "Check {}: {} passed, {} failed, {} ignored",
                step.check_name.as_deref().unwrap_or("unknown"),
                counts.passed,
                counts.failed,
                counts.ignored
            ));
        }
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
            if step.state == "changes_requested" {
                "fixes requested"
            } else {
                step.state.as_str()
            }
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
                waiting_ms: a.waiting_ms as u64,
                usage: a.usage.label(),
                usage_tokens: format!(
                    "I {} R {} W {} O {}",
                    number(a.usage.input),
                    number(a.usage.cache_read),
                    number(a.usage.cache_write),
                    number(a.usage.output)
                ),
                usage_cost: format!("Cost {}", cost_label(a.usage.cost_microusd)),
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
        origin: task
            .origin_task_id
            .zip(task.origin_attempt_id)
            .map(|(task, attempt)| (task as usize, attempt as usize)),
        follow_ups: task
            .follow_ups
            .iter()
            .map(|(id, title)| (*id as usize, title.clone()))
            .collect(),
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

fn revision_overview(revision: &domain::Revision) -> String {
    let definition = &revision.definition;
    let models = domain::ROLES
        .iter()
        .zip(&definition.models)
        .map(|(role, model)| {
            model.as_ref().map_or_else(
                || format!("{role}: unconfigured"),
                |model| {
                    format!(
                        "{role}: {}/{} · effort {}",
                        model.provider,
                        model.model,
                        model.effort.as_deref().unwrap_or("none")
                    )
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Revision {} · {}\nApproval: {} · {}\nGoal: {}\nAcceptance: {}\n{models}\nAttempt limit: {} · Task limit: {} · Stage limit: {}",
        revision.number,
        definition.title,
        revision.lifecycle,
        revision.actor.as_deref().unwrap_or("not recorded"),
        definition.goal,
        definition
            .criteria
            .iter()
            .enumerate()
            .map(|(i, criterion)| format!("{}: {criterion}", i + 1))
            .collect::<Vec<_>>()
            .join("; "),
        definition
            .attempt_limit
            .map_or("unlimited".into(), |limit| limit.to_string()),
        crate::application::limits::label(definition.timeout_ms),
        crate::application::limits::label(definition.stage_timeout_ms),
    )
}

fn task_overview(task: &domain::Task) -> String {
    let cost = if task.attempts.is_empty() {
        None
    } else {
        task.attempts
            .iter()
            .try_fold(0_i64, |sum, a| sum.checked_add(a.usage.cost_microusd?))
    };
    let mut body = format!(
        "Task #{} · {} · {}\nCurrent task definition\n{}\nTask cost: {}",
        task.id,
        task.definition().definition.title,
        task.status,
        revision_overview(task.definition()),
        cost_label(cost)
    );
    for (index, attempt) in task.attempts.iter().enumerate() {
        body.push_str(&format!(
            "\nAttempt {} · revision {} · {} · active {} ms · waiting {} ms\nUsage: {}",
            index + 1,
            attempt.revision,
            attempt.state,
            attempt.active_ms,
            attempt.waiting_ms,
            attempt.usage.label()
        ));
        for role in &attempt.role_usage {
            body.push_str(&format!(
                "\n{} {} / {}: {} · provider elapsed {} ms",
                role.role,
                role.model,
                role.effort.as_deref().unwrap_or("none"),
                role.totals.label(),
                role.provider_elapsed_ms
            ));
        }
    }
    for revision in task.revisions.iter().filter(|revision| {
        revision.lifecycle == "draft" && revision.number > task.definition().number
    }) {
        body.push_str(&format!(
            "\nNewer draft revision {} (needs approval)\n{}",
            revision.number,
            revision_overview(revision)
        ));
    }
    if let Some(reason) = &task.resolution_reason {
        body.push_str(&format!(
            "\nResolution: {} · {reason}",
            task.resolved_by.as_deref().unwrap_or("unknown actor")
        ));
    }
    for (id, title) in &task.follow_ups {
        body.push_str(&format!("\nFollow-up #{id}: {title}"));
    }
    if let (Some(origin), Some(attempt)) = (task.origin_task_id, task.origin_attempt_id) {
        body.push_str(&format!(
            "\nFollow-up from task #{origin}, attempt #{attempt}"
        ));
    }
    body
}

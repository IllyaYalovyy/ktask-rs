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
        app.message("SQLite tasks · Run starts the supervisor · closing the TUI keeps it running");
        Ok(app)
    }
    pub(crate) fn reload(&mut self) -> Result<(), String> {
        let snapshot = self
            .service
            .as_mut()
            .expect("SQLite mode")
            .snapshot()
            .map_err(|e| e.to_string())?;
        let queue = snapshot
            .queue
            .iter()
            .map(project_task)
            .collect::<Result<Vec<_>, _>>()?;
        let history = snapshot
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
        self.queue = queue;
        self.history = history;
        self.events = snapshot.events;
        if self.current().map(|t| t.id) != previous {
            self.follow = true;
            self.log_offset = 0;
        }
        self.reconcile();
        Ok(())
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
        if matches!(action, Action::Breakdown) {
            self.message("AI task breakdown is not connected yet; add well-defined tasks to Queue");
            return true;
        }
        if !self.enabled(action) {
            return false;
        }
        let result = match action {
            Action::Run | Action::Retry | Action::Diagnose => self
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
            Ok(()) => self.committed(match action {
                Action::Run | Action::Retry | Action::Diagnose => {
                    "Supervisor started · close TUI safely · Pause stops execution"
                }
                Action::Pause => "Pause requested · waiting for worker shutdown and cleanup",
                _ => "Saved to SQLite",
            }),
            Err(error) => self.message(error),
        }
        true
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
            | DialogKind::Reject(id) => Some(id),
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
                dialog.body = dialog.body.replace("All tasks, models, runs, and files are simulated.", "Changes save immediately. Run starts Codex; Pause stops workers.\nChecks, review and validation gate success. Failed work uses the remediation model.\nModels: provider/model, optional effort. l: project events.\nTask breakdown is not connected yet.");
            }
            _ => {}
        }
    }

    pub(crate) fn apply_persistent_dialog(&mut self, dialog: &mut Dialog) -> Result<bool, String> {
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
            DialogKind::Answer(_) => {
                self.service
                    .as_mut()
                    .unwrap()
                    .answer(target()?, dialog.value(0))
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
                outcome: a.outcome.clone().unwrap_or_else(|| a.state.clone()),
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

//! Forms translate terminal input into shared application commands.
use crate::{
    app::{Action, App},
    application::{
        ModelConfig,
        configuration::{Defaults, MODEL_ROLES, ProjectCheck},
        preferences::{Continuation, PlannerApproval, Preferences},
    },
    dialog::{Dialog, DialogKind, Field},
};

impl App {
    pub(crate) fn load_preferences(&mut self) -> Result<Preferences, String> {
        let paths = self.instance.paths().map_err(|e| e.to_string())?;
        let saved = self
            .service
            .as_ref()
            .unwrap()
            .preferences(&paths.config)
            .map_err(|e| e.to_string())?;
        self.use_preferences(&saved);
        Ok(saved)
    }
    fn use_preferences(&mut self, saved: &Preferences) {
        self.mouse_enabled = saved.mouse;
        self.review_required = saved.planner_approval == PlannerApproval::ReviewRequired;
        self.auto_advance = saved.continuation == Continuation::Queue;
    }
    pub(crate) fn open_project_config(&mut self, action: Action) -> Result<(), String> {
        let dialog = match action {
            Action::Storage => {
                let preview = self
                    .service
                    .as_ref()
                    .unwrap()
                    .storage_preview()
                    .map_err(|e| e.to_string())?;
                let mut body = format!(
                    "Retained: {} bytes · eligible: {} bytes in {} attempt(s){}\nCleanup keeps task history and all live or unresolved work.\n",
                    preview.usage_bytes,
                    preview.reclaimable_bytes,
                    preview.candidates.len(),
                    if preview.deferred_for_live_work {
                        " · deferred while work is live"
                    } else {
                        ""
                    }
                );
                for candidate in &preview.candidates {
                    body.push_str(&format!(
                        "Task #{} attempt #{}: {} output + {} artifact bytes ({} files; {} pending)\n",
                        candidate.task_id,
                        candidate.attempt_id,
                        candidate.output_bytes,
                        candidate.artifact_bytes,
                        candidate.artifacts,
                        candidate.pending_artifacts
                    ));
                }
                Dialog::new(
                    DialogKind::Storage(preview.clone()),
                    "Storage retention",
                    &body,
                    vec![
                        Field::text(
                            "Retain completed attempts (days)",
                            &preview.policy.days.to_string(),
                        ),
                        Field::choice("Action", &["Save policy", "Apply preview"], "Save policy"),
                    ],
                )
            }
            Action::ProjectModels => {
                let values = self
                    .service
                    .as_ref()
                    .unwrap()
                    .project_defaults()
                    .map_err(|e| e.to_string())?;
                let mut fields: Vec<Field> = MODEL_ROLES
                    .iter()
                    .zip(&values)
                    .flat_map(|(role, value)| {
                        [
                            Field::text(
                                &format!("{role} provider/model"),
                                &value
                                    .as_ref()
                                    .map(|m| format!("{}/{}", m.provider, m.model))
                                    .unwrap_or_default(),
                            ),
                            Field::text(
                                &format!("{role} effort (optional)"),
                                value
                                    .as_ref()
                                    .and_then(|m| m.effort.as_deref())
                                    .unwrap_or(""),
                            ),
                        ]
                    })
                    .collect();
                fields.push(Field::choice(
                    "New projects",
                    &["Keep their defaults", "Start with these"],
                    "Keep their defaults",
                ));
                Dialog::new(
                    DialogKind::ProjectModels(Box::new(values)),
                    "Project model defaults",
                    "New tasks copy these defaults; existing revisions stay pinned.\nBreak down uses the planning model. \"Start with these\" also makes them the defaults of projects you set up later.\nUse provider/model: codex/gpt-6-sol, claude/sonnet, opencode/anthropic/claude-sonnet-5-5.\nBlank clears the default.",
                    fields,
                )
            }
            Action::ProjectChecks => {
                let checks = self
                    .service
                    .as_ref()
                    .unwrap()
                    .project_checks()
                    .map_err(|e| e.to_string())?;
                let mut field = Field::choice("Check to open", &["Add new check"], "Add new check");
                field.choices.extend(checks.iter().map(check_label));
                let body = format!(
                    "Select a check, then Apply to edit or enable/disable.\n{}",
                    if checks.is_empty() {
                        "No checks configured.".into()
                    } else {
                        checks
                            .iter()
                            .map(|c| format!("{}: {}", check_label(c), c.command))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                );
                Dialog::new(
                    DialogKind::ProjectChecks(checks),
                    "Project checks",
                    &body,
                    vec![field],
                )
            }
            _ => {
                let saved = self.load_preferences()?;
                let network = self
                    .service
                    .as_ref()
                    .ok_or("SQLite mode required")?
                    .agent_network()
                    .map_err(|e| e.to_string())?;
                let fields = vec![
                    Field::choice(
                        "Mouse",
                        &["Enabled", "Disabled"],
                        if saved.mouse { "Enabled" } else { "Disabled" },
                    ),
                    Field::choice(
                        "Planner approval",
                        &["Automatic", "Review required"],
                        if self.review_required {
                            "Review required"
                        } else {
                            "Automatic"
                        },
                    ),
                    Field::choice(
                        "Run mode",
                        &["Run one", "Continue queue"],
                        if self.auto_advance {
                            "Continue queue"
                        } else {
                            "Run one"
                        },
                    ),
                    Field::choice(
                        "Agent network",
                        &["Off", "On"],
                        if network { "On" } else { "Off" },
                    ),
                ];
                Dialog::new(
                    DialogKind::Preferences(saved),
                    "Project preferences",
                    "Saved per project in this instance. Keyboard stays available.\nThis policy controls new breakdown and repair tasks.\nRun mode applies to the next Run/resume/retry.\nAgent network On lets Codex agents that change the project use the network, for\nexample to install dependencies; Off runs them offline.",
                    fields,
                )
            }
        };
        self.dialog = Some(dialog);
        Ok(())
    }
    pub(crate) fn save_project_config(&mut self, dialog: &mut Dialog) -> Result<bool, String> {
        match &dialog.kind {
            DialogKind::Storage(preview) => {
                let days: i64 = dialog
                    .value(0)
                    .parse()
                    .map_err(|_| "Days must be a nonnegative integer")?;
                if dialog.value(1) == "Apply preview" {
                    if days != preview.policy.days {
                        return Err(
                            "Save the changed policy, then reopen Storage to preview it".into()
                        );
                    }
                    let body = format!(
                        "Delete {} bytes of output and transcripts from {} completed attempt(s)?\nThis cannot be undone. Task history stays. Cancel keeps the evidence.\n\n{}",
                        preview.reclaimable_bytes,
                        preview.candidates.len(),
                        preview
                            .candidates
                            .iter()
                            .map(|candidate| format!(
                                "Task #{} attempt #{}: {} output + {} artifact bytes",
                                candidate.task_id,
                                candidate.attempt_id,
                                candidate.output_bytes,
                                candidate.artifact_bytes
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    dialog.kind = DialogKind::StorageConfirm(preview.clone());
                    dialog.title = "Delete stored evidence?".into();
                    dialog.body = body;
                    dialog.fields.clear();
                    dialog.focus = 0;
                    dialog.scroll = 0;
                    dialog.error.clear();
                    return Ok(false);
                }
                self.service
                    .as_mut()
                    .unwrap()
                    .set_retention(preview.policy, days)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::StorageConfirm(preview) => {
                let applied = self
                    .service
                    .as_mut()
                    .unwrap()
                    .cleanup_storage(preview)
                    .map_err(|e| e.to_string())?;
                self.message(format!(
                    "Cleaned {} attempt(s), {} bytes",
                    applied.candidates.len(),
                    applied.reclaimable_bytes
                ));
                return Ok(true);
            }
            DialogKind::ProjectModels(expected) => {
                let mut values: Defaults = std::array::from_fn(|_| None);
                for (i, value) in values.iter_mut().enumerate() {
                    *value =
                        ModelConfig::from_fields(dialog.value(i * 2), dialog.value(i * 2 + 1))?;
                }
                let service = self.service.as_mut().unwrap();
                service
                    .save_defaults(expected, &values)
                    .map_err(|e| e.to_string())?;
                if dialog.value(values.len() * 2) == "Start with these" {
                    service
                        .save_new_project_defaults(&values)
                        .map_err(|e| e.to_string())?;
                }
            }
            DialogKind::ProjectChecks(checks) => {
                let selected = dialog.fields[0]
                    .choices
                    .iter()
                    .position(|c| c == dialog.value(0))
                    .ok_or("Select a check")?;
                // A project's first check starts from the command its files suggest.
                let detected = (selected == 0 && checks.is_empty())
                    .then(|| self.service.as_ref()?.project_root().ok())
                    .flatten()
                    .and_then(|root| crate::runner::setup::detect_check(&root));
                let value = if selected == 0 {
                    ProjectCheck {
                        id: None,
                        name: detected.as_ref().map_or("", |_| "check").into(),
                        command: detected.as_ref().map_or_else(String::new, |d| d.0.clone()),
                        directory: ".".into(),
                        timeout_ms: 600_000,
                        enabled: true,
                    }
                } else {
                    checks[selected - 1].clone()
                };
                let fields = vec![
                    Field::text("Name", &value.name),
                    Field::text("Command", &value.command),
                    Field::text("Working directory (relative)", &value.directory),
                    Field::text("Timeout (ms)", &value.timeout_ms.to_string()),
                    Field::choice(
                        "State",
                        &["Enabled", "Disabled"],
                        if value.enabled { "Enabled" } else { "Disabled" },
                    ),
                ];
                *dialog = Dialog::new(
                    DialogKind::ProjectCheck {
                        expected: checks.clone(),
                        id: value.id,
                    },
                    "Edit project check",
                    &format!(
                        "Save validates all fields together. Cancel changes nothing.\nChecks cannot change during running, paused or waiting attempts.{}",
                        detected.map_or(String::new(), |(_, file)| format!(
                            "\nCommand suggested from {file}; change it if your project checks differently."
                        ))
                    ),
                    fields,
                );
                return Ok(false);
            }
            DialogKind::ProjectCheck { expected, id } => {
                let value = ProjectCheck {
                    id: *id,
                    name: dialog.value(0).into(),
                    command: dialog.value(1).into(),
                    directory: dialog.value(2).into(),
                    timeout_ms: dialog
                        .value(3)
                        .parse()
                        .map_err(|_| "Timeout must be a positive integer")?,
                    enabled: dialog.value(4) == "Enabled",
                };
                self.service
                    .as_mut()
                    .unwrap()
                    .save_check(expected, &value)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Preferences(expected) => {
                let value = Preferences {
                    version: expected.version,
                    mouse: dialog.value(0) == "Enabled",
                    planner_approval: if dialog.value(1) == "Review required" {
                        PlannerApproval::ReviewRequired
                    } else {
                        PlannerApproval::Automatic
                    },
                    continuation: if dialog.value(2) == "Continue queue" {
                        Continuation::Queue
                    } else {
                        Continuation::Single
                    },
                };
                let paths = self.instance.paths().map_err(|e| e.to_string())?;
                let service = self.service.as_mut().unwrap();
                let saved = service
                    .save_preferences(&paths.config, expected, &value)
                    .map_err(|e| e.to_string())?;
                service
                    .set_agent_network(dialog.value(3) == "On")
                    .map_err(|e| e.to_string())?;
                self.use_preferences(&saved);
            }
            _ => return Err("Not a project configuration form".into()),
        }
        self.message("Project configuration saved");
        Ok(true)
    }
}
fn check_label(check: &ProjectCheck) -> String {
    format!(
        "{} [{}]",
        check.name,
        if check.enabled { "enabled" } else { "disabled" }
    )
}

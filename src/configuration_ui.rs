//! Forms translate terminal input into shared application commands.
use crate::{
    app::{Action, Screen},
    application::{
        ModelConfig,
        configuration::{Defaults, DeliveryConfig, MODEL_ROLES, ProjectCheck},
        preferences::{Continuation, PlannerApproval, Preferences},
    },
    dialog::{Dialog, DialogKind, Field},
    forms,
    persistent::StoredWorkspace,
};

const DAYS: &str = "Retain completed attempts (days)";
const STORAGE_ACTION: &str = "Action";
const MAX_BYTES: &str = "Maximum allocated bytes";
const MAX_FILES: &str = "Maximum files and directories";
const NEW_PROJECTS: &str = "New projects";
const CHECK_TO_OPEN: &str = "Check to open";
const NAME: &str = "Name";
const COMMAND: &str = "Command";
const DIRECTORY: &str = "Working directory (relative)";
const TIMEOUT: &str = "Timeout (ms)";
const STATE: &str = "State";
const MODE: &str = "Mode";
const REMOTE: &str = "Git remote";
const BASE: &str = "Base branch";
const RUN_MODE: &str = "Run mode";
const NETWORK: &str = "Agent network";
const CHECK_FIRST: &str = "Check before task";

fn model_label(role: &str) -> String {
    format!("{role} provider/model")
}
fn effort_label(role: &str) -> String {
    format!("{role} effort (optional)")
}

impl StoredWorkspace {
    pub(crate) fn load_preferences(&mut self, screen: &mut Screen) -> Result<Preferences, String> {
        let paths = screen.instance.paths().map_err(|e| e.to_string())?;
        let saved = self
            .service
            .preferences(&paths.config)
            .map_err(|e| e.to_string())?;
        self.use_preferences(screen, &saved);
        Ok(saved)
    }
    fn use_preferences(&mut self, screen: &mut Screen, saved: &Preferences) {
        screen.mouse_enabled = saved.mouse;
        self.review_required = saved.planner_approval == PlannerApproval::ReviewRequired;
        screen.auto_advance = saved.continuation == Continuation::Queue;
    }
    pub(crate) fn open_project_config(
        &mut self,
        screen: &mut Screen,
        action: Action,
    ) -> Result<Dialog, String> {
        if action == Action::BuildCache {
            return self.open_build_cache(screen);
        }
        Ok(match action {
            Action::Storage => {
                let preview = self.service.storage_preview().map_err(|e| e.to_string())?;
                let mut body = format!(
                    "Retained: {} logical bytes · eligible: {} bytes in {} attempt(s){}\nAllocated: {} bytes in {} files/directories · eligible files: {} · reusable SQLite: {} · protected: {} bytes{}\nSQLite: {} · WAL: {} · runner logs: {} · evidence: {} · directories: {}\nProtected evidence: {} bytes younger than the {}-day minimum age · {} bytes referenced by reused checks · {} bytes of open work\nCleanup keeps task history and all live or unresolved work.\n",
                    preview.usage_bytes,
                    preview.reclaimable_bytes,
                    preview.candidates.len(),
                    if preview.deferred_for_live_work {
                        " · deferred while work is live"
                    } else {
                        ""
                    },
                    preview.allocated_bytes,
                    preview.allocated_files,
                    preview.eligible_file_bytes,
                    preview.reusable_database_bytes,
                    preview.protected_bytes,
                    if preview.launch_capacity_reached {
                        " · WORK PAUSED AT CAPACITY RESERVE"
                    } else {
                        ""
                    },
                    preview.database_bytes,
                    preview.wal_bytes,
                    preview.runner_log_bytes,
                    preview.evidence_bytes,
                    preview.directory_entries,
                    preview.protected_by_age_bytes,
                    preview.policy.days,
                    preview.protected_by_reference_bytes,
                    preview.protected_open_bytes,
                );
                if let Some(note) = &preview.vacuum_note {
                    body.push_str(&format!("{note}.\n"));
                }
                if preview.effective_max_bytes != preview.policy.max_bytes
                    || preview.effective_max_files != preview.policy.max_files
                {
                    body.push_str(&format!(
                        "Shared effective limit: {} bytes, {} files.\n",
                        preview.effective_max_bytes, preview.effective_max_files
                    ));
                }
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
                        Field::text(DAYS, &preview.policy.days.to_string()),
                        Field::choice(
                            STORAGE_ACTION,
                            &["Save policy", "Apply preview"],
                            "Save policy",
                        ),
                        Field::text(MAX_BYTES, &preview.policy.max_bytes.to_string()),
                        Field::text(MAX_FILES, &preview.policy.max_files.to_string()),
                    ],
                )
            }
            Action::ProjectModels => {
                let values = self.service.project_defaults().map_err(|e| e.to_string())?;
                let mut fields: Vec<Field> = MODEL_ROLES
                    .iter()
                    .zip(&values)
                    .flat_map(|(role, value)| {
                        [
                            Field::text(
                                &model_label(role),
                                &value
                                    .as_ref()
                                    .map(ModelConfig::identifier)
                                    .unwrap_or_default(),
                            ),
                            Field::text(
                                &effort_label(role),
                                value
                                    .as_ref()
                                    .and_then(|m| m.effort.as_deref())
                                    .unwrap_or(""),
                            ),
                        ]
                    })
                    .collect();
                fields.push(Field::choice(
                    NEW_PROJECTS,
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
                let checks = self.service.project_checks().map_err(|e| e.to_string())?;
                let mut field = Field::choice(CHECK_TO_OPEN, &["Add new check"], "Add new check");
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
            Action::Delivery => {
                let delivery = self.service.delivery_config().map_err(|e| e.to_string())?;
                Dialog::new(
                    DialogKind::Delivery(delivery.clone()),
                    "Git delivery",
                    "Local commits only. Direct pushes verified commits to the base branch. Pull request uses a project branch and updates one PR. Remote and base default to origin/main.",
                    vec![
                        Field::choice(
                            MODE,
                            &["Local", "Direct push", "Pull request"],
                            match delivery.mode.as_str() {
                                "direct" => "Direct push",
                                "pr" => "Pull request",
                                _ => "Local",
                            },
                        ),
                        Field::text(REMOTE, &delivery.remote),
                        Field::text(BASE, &delivery.base_branch),
                    ],
                )
            }
            _ => {
                let saved = self.load_preferences(screen)?;
                let service = &self.service;
                let network = service.agent_network().map_err(|e| e.to_string())?;
                let verify_first = service.check_before_task().map_err(|e| e.to_string())?;
                let fields = vec![
                    forms::mouse(saved.mouse),
                    forms::planner_approval(self.review_required),
                    Field::choice(
                        RUN_MODE,
                        &["Run one", "Continue queue"],
                        if screen.auto_advance {
                            "Continue queue"
                        } else {
                            "Run one"
                        },
                    ),
                    Field::choice(NETWORK, &["Off", "On"], if network { "On" } else { "Off" }),
                    Field::choice(
                        CHECK_FIRST,
                        &["On", "Off"],
                        if verify_first { "On" } else { "Off" },
                    ),
                ];
                Dialog::new(
                    DialogKind::Preferences(saved),
                    "Project preferences",
                    "Saved per project in this instance. Keyboard stays available.\nThis policy controls new breakdown and repair tasks.\nRun mode applies to the next Run/resume/retry.\nAgent network applies to execution and repair workers. On permits network access with Codex, Claude Code and OpenCode. Off is enforced by Codex; Claude Code and OpenCode cannot enforce Off, so their writable turns are blocked.\nCheck before task runs the project checks on a task's clean starting commit; a red start stops it before any tokens are spent.",
                    fields,
                )
            }
        })
    }
    pub(crate) fn save_project_config(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
    ) -> Result<bool, String> {
        if matches!(
            dialog.kind,
            DialogKind::BuildCache(_) | DialogKind::BuildCacheConfirm(_)
        ) {
            return self.save_build_cache(screen, dialog);
        }
        match &dialog.kind {
            DialogKind::Storage(preview) => {
                let days: i64 = dialog
                    .get(DAYS)
                    .parse()
                    .map_err(|_| "Days must be a nonnegative integer")?;
                let max_bytes: i64 = dialog
                    .get(MAX_BYTES)
                    .parse()
                    .map_err(|_| "Bytes must be an integer")?;
                let max_files: i64 = dialog
                    .get(MAX_FILES)
                    .parse()
                    .map_err(|_| "Files must be an integer")?;
                if dialog.get(STORAGE_ACTION) == "Apply preview" {
                    if days != preview.policy.days
                        || max_bytes != preview.policy.max_bytes
                        || max_files != preview.policy.max_files
                    {
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
                    .set_storage_policy(preview.policy, days, max_bytes, max_files)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::StorageConfirm(preview) => {
                let applied = self
                    .service
                    .cleanup_storage(preview)
                    .map_err(|e| e.to_string())?;
                screen.message(format!(
                    "Cleaned {} attempt(s), {} bytes{}",
                    applied.attempts,
                    applied.bytes,
                    if applied.batch_limit_reached {
                        " · batch limit reached; reopen Storage for more"
                    } else {
                        ""
                    }
                ));
                return Ok(true);
            }
            DialogKind::ProjectModels(expected) => {
                let mut values: Defaults = std::array::from_fn(|_| None);
                for (role, value) in MODEL_ROLES.iter().zip(values.iter_mut()) {
                    *value = ModelConfig::from_fields(
                        dialog.get(&model_label(role)),
                        dialog.get(&effort_label(role)),
                    )?;
                }
                let service = &mut self.service;
                service
                    .save_defaults(expected, &values)
                    .map_err(|e| e.to_string())?;
                if dialog.get(NEW_PROJECTS) == "Start with these" {
                    service
                        .save_new_project_defaults(&values)
                        .map_err(|e| e.to_string())?;
                }
            }
            DialogKind::ProjectChecks(checks) => {
                let chosen = dialog.get(CHECK_TO_OPEN);
                let selected = dialog
                    .fields
                    .iter()
                    .find(|field| field.label == CHECK_TO_OPEN)
                    .and_then(|field| field.choices.iter().position(|c| c == chosen))
                    .ok_or("Select a check")?;
                // A project's first check starts from the command its files suggest.
                let detected = (selected == 0 && checks.is_empty())
                    .then(|| self.service.project_root().ok())
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
                    Field::text(NAME, &value.name),
                    Field::text(COMMAND, &value.command),
                    Field::text(DIRECTORY, &value.directory),
                    Field::text(TIMEOUT, &value.timeout_ms.to_string()),
                    Field::choice(
                        STATE,
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
                    name: dialog.get(NAME).into(),
                    command: dialog.get(COMMAND).into(),
                    directory: dialog.get(DIRECTORY).into(),
                    timeout_ms: dialog
                        .get(TIMEOUT)
                        .parse()
                        .map_err(|_| "Timeout must be a positive integer")?,
                    enabled: dialog.get(STATE) == "Enabled",
                };
                self.service
                    .save_check(expected, &value)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Delivery(expected) => {
                let value = DeliveryConfig {
                    version: expected.version,
                    mode: match dialog.get(MODE) {
                        "Direct push" => "direct",
                        "Pull request" => "pr",
                        _ => "local",
                    }
                    .into(),
                    remote: dialog.get(REMOTE).into(),
                    base_branch: dialog.get(BASE).into(),
                };
                self.service
                    .save_delivery_config(expected, &value)
                    .map_err(|e| e.to_string())?;
            }
            DialogKind::Preferences(expected) => {
                let value = Preferences {
                    version: expected.version,
                    mouse: dialog.get(forms::MOUSE) == "Enabled",
                    planner_approval: if dialog.get(forms::PLANNER_APPROVAL) == "Review required" {
                        PlannerApproval::ReviewRequired
                    } else {
                        PlannerApproval::Automatic
                    },
                    continuation: if dialog.get(RUN_MODE) == "Continue queue" {
                        Continuation::Queue
                    } else {
                        Continuation::Single
                    },
                };
                let paths = screen.instance.paths().map_err(|e| e.to_string())?;
                let service = &mut self.service;
                let saved = service
                    .save_preferences(&paths.config, expected, &value)
                    .map_err(|e| e.to_string())?;
                service
                    .set_agent_network(dialog.get(NETWORK) == "On")
                    .map_err(|e| e.to_string())?;
                service
                    .set_check_before_task(dialog.get(CHECK_FIRST) == "On")
                    .map_err(|e| e.to_string())?;
                self.use_preferences(screen, &saved);
            }
            _ => return Err("Not a project configuration form".into()),
        }
        screen.message("Project configuration saved");
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

//! Forms translate terminal input into shared application commands.
use crate::{
    app::{Action, Screen},
    application::{
        ModelConfig,
        configuration::{Defaults, DeliveryConfig, MODEL_ROLES, ProjectCheck},
        preferences::{Continuation, PlannerApproval, Preferences},
        provision::Plan,
        user_defaults::{DefaultsRow, UserDefaults},
    },
    dialog::{Dialog, DialogKind, Field},
    forms,
    persistent::StoredWorkspace,
};

const DAYS: &str = "Retain completed attempts (days)";
const STORAGE_ACTION: &str = "Action";
const MAX_BYTES: &str = "Maximum allocated bytes";
const MAX_FILES: &str = "Maximum files and directories";
pub(crate) const DEFAULT_PROFILE: &str = "Default profile";
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
const BASELINE: &str = "Baseline checks";
const PLANNER: &str = "Planner approval";
const AFTER_SAVE: &str = "After saving";
const SAVE_ONLY: &str = "Only save";
const SAVE_PREVIEW: &str = "Preview applying to this project";
const PROJECT_SETTINGS: &str = "Project settings";
const KEEP_SETTINGS: &str = "Keep project settings";
const APPLY_SETTINGS: &str = "Apply user defaults";
const TOOL_ACTION: &str = "Tool action";
const TOOL_NAME: &str = "Tool name";
const TOOL_VERSION: &str = "Tool version";
const TOOL_PLATFORM: &str = "Platform";
const TOOL_ARCH: &str = "Architecture";
const TOOL_PATH: &str = "Installation path";
const TOOL_COMPONENTS: &str = "Components (; in one component, comma between components)";
const TOOL_OWNERSHIP: &str = "Ownership";
const TOOL_GENERATION: &str = "Generation ID";
const TOOL_ROW_VERSION: &str = "Expected version (binding or generation)";

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
            Action::Tools => {
                let tools = self.service.tools().map_err(|e| e.to_string())?;
                let source = tools
                    .iter()
                    .find(|tool| tool.name == "android-sdk" && tool.provenance == "external")
                    .map(|tool| tool.id.to_string())
                    .unwrap_or_default();
                let mut body: String = if tools.is_empty() {
                    "No registered tool generations. Register an existing installation, then bind its generation to this project.".into()
                } else {
                    tools
                        .iter()
                        .map(|t| {
                            format!(
                                "#{} v{} · {} {} · {} {} · {} · {} · {}{} · components: {}\n",
                                t.id,
                                t.row_version,
                                t.name,
                                t.version,
                                t.platform,
                                t.architecture,
                                t.path.display(),
                                t.provenance,
                                if t.ready {
                                    "ready"
                                } else {
                                    "changed or missing"
                                },
                                t.binding_version
                                    .map_or(String::new(), |v| format!(" · bound v{v}")),
                                t.components.join(", ")
                            )
                        })
                        .collect()
                };
                body.push_str(&format!("\nProvisioning: {} (manual is the default). Select Plan for packages, source, destination, capacity and license status.\n", if self.service.provision_enabled().map_err(|e| e.to_string())? { "automatic enabled" } else { "manual" }));
                Dialog::new(
                    DialogKind::Tools,
                    "Project tools",
                    &body,
                    vec![
                        Field::choice(
                            TOOL_ACTION,
                            &[
                                "Register",
                                "Bind",
                                "Unbind",
                                "Refresh external",
                                "Plan",
                                "Enable automatic",
                                "Use manual",
                                "Consent",
                                "Install",
                                "Progress",
                                "Cancel install",
                            ],
                            "Register",
                        ),
                        Field::text(TOOL_NAME, ""),
                        Field::text(TOOL_VERSION, ""),
                        Field::text(TOOL_PLATFORM, std::env::consts::OS),
                        Field::text(TOOL_ARCH, std::env::consts::ARCH),
                        Field::text(TOOL_PATH, ""),
                        Field::text(TOOL_COMPONENTS, ""),
                        Field::choice(TOOL_OWNERSHIP, &["External", "Managed"], "External"),
                        Field::text(TOOL_GENERATION, &source),
                        Field::text(TOOL_ROW_VERSION, "0"),
                    ],
                )
            }
            Action::Storage => {
                let preview = self.service.storage_preview().map_err(|e| e.to_string())?;
                let mut body = format!(
                    "Retained: {} logical bytes · eligible: {} bytes in {} attempt(s){}\nArtifacts: {} logical bytes · {} distinct bytes\nAllocated: {} bytes in {} files/directories · eligible files: {} · reusable SQLite: {} · protected: {} bytes{}\nSQLite: {} · WAL: {} · runner logs: {} · evidence: {} · directories: {}\nProtected evidence: {} bytes younger than the {}-day minimum age · {} bytes referenced by reused checks · {} bytes of open work\nOther links retain {} allocated bytes after eligible paths are removed.\n",
                    preview.usage_bytes,
                    preview.reclaimable_bytes,
                    preview.candidates.len(),
                    if preview.deferred_for_live_work {
                        " · deferred while work is live"
                    } else {
                        ""
                    },
                    preview.logical_artifact_bytes,
                    preview.distinct_artifact_bytes,
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
                    preview.protected_shared_file_bytes,
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
                let values = self
                    .service
                    .project_overrides()
                    .map_err(|e| e.to_string())?;
                let effective = self.service.effective_models().map_err(|e| e.to_string())?;
                let mut fields = model_fields(&values);
                fields.push(Field::choice(
                    NEW_PROJECTS,
                    &["Keep their defaults", "Start with these"],
                    "Keep their defaults",
                ));
                Dialog::new(
                    DialogKind::ProjectModels(Box::new(values)),
                    "Project model defaults",
                    &format!(
                        "Effective models for tasks defined now, and where each comes from (user defaults, then the selected profile, then these overrides):\n{}\nNew tasks copy these; existing revisions stay pinned. Break down uses the planning model.\nThe fields below are this project's own overrides: blank inherits the profile or user default. \"Start with these\" also makes the models you set the user defaults for every project.\nUse provider/model: codex/gpt-6-sol, claude/sonnet, opencode/anthropic/claude-sonnet-5-5.",
                        crate::profile_ui::effective_text(&effective)
                    ),
                    fields,
                )
            }
            Action::UserDefaults => {
                let saved = self.service.user_defaults().map_err(|e| e.to_string())?;
                let models = self
                    .service
                    .new_project_defaults()
                    .map_err(|e| e.to_string())?;
                let mut fields = model_fields(&models);
                fields.extend([
                    Field::choice(NETWORK, &["Off", "On"], switch_label(saved.agent_network)),
                    Field::choice(
                        BASELINE,
                        &["On", "Off"],
                        switch_label(saved.check_before_task),
                    ),
                    Field::text(TIMEOUT, &saved.check_timeout_ms.to_string()),
                    Field::choice(
                        RUN_MODE,
                        &["Run one", "Continue queue"],
                        if saved.continuation == Continuation::Queue {
                            "Continue queue"
                        } else {
                            "Run one"
                        },
                    ),
                    Field::choice(
                        PLANNER,
                        &["Automatic", "Review required"],
                        if saved.planner_approval == PlannerApproval::ReviewRequired {
                            "Review required"
                        } else {
                            "Automatic"
                        },
                    ),
                    Field::choice(
                        MODE,
                        &["Local", "Direct push", "Pull request"],
                        delivery_label(&saved.delivery_mode),
                    ),
                    Field::choice(AFTER_SAVE, &[SAVE_ONLY, SAVE_PREVIEW], SAVE_ONLY),
                    crate::profile_ui::default_profile_field(
                        &self.service,
                        saved.default_profile.as_deref(),
                    )?,
                ]);
                Dialog::new(
                    DialogKind::UserDefaults(saved),
                    "User defaults for new projects",
                    "Saved once for this user and instance. A project registered later starts with them and selects the default profile; existing projects, tasks and attempts keep their own settings.\nBlank model clears a role. A profile or project override takes precedence over these models. Never inherited: the check command, directories, remote and base branch.\nChoose \"Preview applying to this project\" to see this project's values beside these before anything changes.",
                    fields,
                )
            }
            Action::Profiles => self.open_profiles()?,
            Action::Categories => self.open_categories()?,
            Action::Lenses => self.open_lenses()?,
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
                            delivery_label(&delivery.mode),
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
                    "Saved per project in this instance. Keyboard stays available.\nThis policy controls new breakdown and repair tasks.\nRun mode applies to the next Run/resume/retry.\nAgent network applies to execution and repair workers. On permits network access with Codex, Claude Code and OpenCode. Off is enforced by Codex; Claude Code and OpenCode cannot enforce Off, so their writable turns are blocked.\nCheck before task runs the project checks on a task's clean starting commit; a red start enters bounded automatic repair with the original task goal.",
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
            DialogKind::Tools => {
                let paths = screen.instance.paths().map_err(|e| e.to_string())?;
                if dialog.get(TOOL_ACTION) == "Plan" {
                    let source: i64 = dialog
                        .get(TOOL_GENERATION)
                        .parse()
                        .map_err(|_| "Source generation ID must be a number")?;
                    let plan = self
                        .service
                        .provision_plan(&paths, crate::application::provision::RECIPE, source)
                        .map_err(|e| e.to_string())?;
                    dialog.body = install_plan_text(&plan);
                    dialog.kind = DialogKind::ToolPreview;
                    dialog.title = "Android install plan".into();
                    dialog.fields.clear();
                    dialog.scroll = 0;
                    return Ok(false);
                }
                if dialog.get(TOOL_ACTION) == "Progress" {
                    let status = self
                        .service
                        .provision_status(&paths)
                        .map_err(|e| e.to_string())?;
                    dialog.body = match status {
                        Some(status) => {
                            serde_json::to_string_pretty(&status).map_err(|e| e.to_string())?
                        }
                        None => "No Android installation has started for this project.".into(),
                    };
                    dialog.kind = DialogKind::ToolPreview;
                    dialog.title = "Android install progress".into();
                    dialog.fields.clear();
                    dialog.scroll = 0;
                    return Ok(false);
                }
                if dialog.get(TOOL_ACTION) == "Cancel install" {
                    if let Some(job) = &self.provision_job {
                        job.cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        self.service
                            .cancel_provision(&paths)
                            .map_err(|e| e.to_string())?;
                    }
                    screen.message("Provisioning cancellation requested");
                    return Ok(true);
                }
                if matches!(dialog.get(TOOL_ACTION), "Enable automatic" | "Use manual") {
                    let enabled = dialog.get(TOOL_ACTION) == "Enable automatic";
                    self.service
                        .set_provision_enabled(enabled)
                        .map_err(|e| e.to_string())?;
                    screen.message(if enabled {
                        "Provisioning enabled"
                    } else {
                        "Manual tool setup selected"
                    });
                    return Ok(true);
                }
                if dialog.get(TOOL_ACTION) == "Install" {
                    if self.provision_job.is_some() {
                        return Err("An Android installer is already running in this window".into());
                    }
                    let source: i64 = dialog
                        .get(TOOL_GENERATION)
                        .parse()
                        .map_err(|_| "Source generation ID must be a number")?;
                    let recipe = crate::application::provision::RECIPE.to_owned();
                    let root = self.service.project_root().map_err(|e| e.to_string())?;
                    let waiting_task =
                        self.service.provision_target().map_err(|e| e.to_string())?;
                    let instance = screen.instance;
                    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let signal = std::sync::Arc::clone(&cancelled);
                    let handle = std::thread::spawn(move || {
                        let paths = instance.paths().map_err(|e| e.to_string())?;
                        let database =
                            crate::storage::Database::open(&paths).map_err(|e| e.to_string())?;
                        let mut service = crate::application::Service::open(database, &root, None)
                            .map_err(|e| e.to_string())?;
                        service
                            .provision_android_cancelable(&paths, &recipe, source, &signal)
                            .map_err(|e| e.to_string())
                    });
                    self.provision_job = Some(crate::persistent::ProvisionJob {
                        cancelled,
                        handle: Some(handle),
                        waiting_task,
                    });
                    screen.message(
                        "Android installer started; Tools shows progress and cancellation",
                    );
                    return Ok(true);
                }
                if dialog.get(TOOL_ACTION) == "Consent" {
                    let source: i64 = dialog
                        .get(TOOL_GENERATION)
                        .parse()
                        .map_err(|_| "Source generation ID must be a number")?;
                    let result = self
                        .service
                        .consent_to_provision(crate::application::provision::RECIPE, source)
                        .map(|_| "Recorded human license consent".to_string())
                        .map_err(|e| e.to_string())?;
                    screen.message(result);
                    return Ok(true);
                }
                let components: Vec<String> = dialog
                    .get(TOOL_COMPONENTS)
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                let result = match dialog.get(TOOL_ACTION) {
                    "Register" => self
                        .service
                        .register_tool(
                            &paths,
                            &crate::application::tools::Registration {
                                name: dialog.get(TOOL_NAME),
                                version: dialog.get(TOOL_VERSION),
                                platform: dialog.get(TOOL_PLATFORM),
                                architecture: dialog.get(TOOL_ARCH),
                                components: &components,
                                path: std::path::Path::new(dialog.get(TOOL_PATH)),
                                managed: dialog.get(TOOL_OWNERSHIP) == "Managed",
                            },
                        )
                        .map(|id| format!("Registered tool generation #{id}")),
                    "Bind" => {
                        let id: i64 = dialog
                            .get(TOOL_GENERATION)
                            .parse()
                            .map_err(|_| "Generation ID must be a number")?;
                        let version: i64 = dialog
                            .get(TOOL_ROW_VERSION)
                            .parse()
                            .map_err(|_| "Expected version must be a number")?;
                        self.service
                            .bind_tool(dialog.get(TOOL_NAME), id, Some(version))
                            .map(|v| format!("Bound {} to #{id} v{v}", dialog.get(TOOL_NAME)))
                    }
                    "Unbind" => {
                        let version: i64 = dialog
                            .get(TOOL_ROW_VERSION)
                            .parse()
                            .map_err(|_| "Expected version must be a number")?;
                        self.service
                            .unbind_tool(dialog.get(TOOL_NAME), version)
                            .map(|()| format!("Unbound {}", dialog.get(TOOL_NAME)))
                    }
                    _ => {
                        let id: i64 = dialog
                            .get(TOOL_GENERATION)
                            .parse()
                            .map_err(|_| "Generation ID must be a number")?;
                        let version: i64 = dialog
                            .get(TOOL_ROW_VERSION)
                            .parse()
                            .map_err(|_| "Expected version must be a number")?;
                        self.service
                            .refresh_external_tool(
                                &paths,
                                id,
                                version,
                                dialog.get(TOOL_VERSION),
                                &components,
                            )
                            .map(|v| format!("Refreshed generation #{id} v{v}"))
                    }
                }
                .map_err(|e| e.to_string())?;
                screen.message(result);
                return Ok(true);
            }
            DialogKind::ToolPreview => return Ok(true),
            DialogKind::Profiles(_) | DialogKind::Profile { .. } => {
                return self.save_profile_form(screen, dialog);
            }
            DialogKind::Categories(_) | DialogKind::Category { .. } => {
                return self.save_category_form(screen, dialog);
            }
            DialogKind::Lenses(_) | DialogKind::Lens { .. } | DialogKind::LensDefaults(_) => {
                return self.save_lens_form(screen, dialog);
            }
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
                let values = models_from(dialog)?;
                let service = &mut self.service;
                service
                    .save_project_overrides(expected, &values)
                    .map_err(|e| e.to_string())?;
                if dialog.get(NEW_PROJECTS) == "Start with these" {
                    service
                        .merge_new_project_defaults(&values)
                        .map_err(|e| e.to_string())?;
                }
            }
            DialogKind::UserDefaults(expected) => {
                let models = models_from(dialog)?;
                let value = UserDefaults {
                    version: expected.version,
                    agent_network: dialog.get(NETWORK) == "On",
                    check_before_task: dialog.get(BASELINE) == "On",
                    check_timeout_ms: dialog
                        .get(TIMEOUT)
                        .trim()
                        .parse()
                        .map_err(|_| "Check timeout must be a positive integer (ms)")?,
                    continuation: if dialog.get(RUN_MODE) == "Continue queue" {
                        Continuation::Queue
                    } else {
                        Continuation::Single
                    },
                    planner_approval: if dialog.get(PLANNER) == "Review required" {
                        PlannerApproval::ReviewRequired
                    } else {
                        PlannerApproval::Automatic
                    },
                    delivery_mode: delivery_mode_from(dialog.get(MODE)).into(),
                    default_profile: crate::profile_ui::chosen_profile(dialog.get(DEFAULT_PROFILE)),
                };
                self.service
                    .save_new_project_setup(expected, &value, &models)
                    .map_err(|e| e.to_string())?;
                if dialog.get(AFTER_SAVE) == SAVE_PREVIEW {
                    let paths = screen.instance.paths().map_err(|e| e.to_string())?;
                    let rows = self
                        .service
                        .defaults_preview(&paths.config)
                        .map_err(|e| e.to_string())?;
                    *dialog = Dialog::new(
                        DialogKind::ApplyDefaults(rows.clone()),
                        "Apply user defaults to this project?",
                        &format!(
                            "Defaults saved for new projects. This project keeps its settings unless you apply them.\nApplying keeps check commands, remote and base branch, and changes no existing task or attempt.\n\n{}",
                            preview_text(&rows)
                        ),
                        vec![Field::choice(
                            PROJECT_SETTINGS,
                            &[KEEP_SETTINGS, APPLY_SETTINGS],
                            KEEP_SETTINGS,
                        )],
                    );
                    screen.message("User defaults saved");
                    return Ok(false);
                }
            }
            DialogKind::ApplyDefaults(expected) => {
                if dialog.get(PROJECT_SETTINGS) == APPLY_SETTINGS {
                    let paths = screen.instance.paths().map_err(|e| e.to_string())?;
                    let service = &mut self.service;
                    service
                        .apply_user_defaults(&paths.config, expected)
                        .map_err(|e| e.to_string())?;
                    let saved = service
                        .preferences(&paths.config)
                        .map_err(|e| e.to_string())?;
                    self.use_preferences(screen, &saved);
                    screen.message("User defaults applied to this project");
                    return Ok(true);
                }
                screen.message("Project settings kept");
                return Ok(true);
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
                        timeout_ms: self
                            .service
                            .user_defaults()
                            .map_err(|e| e.to_string())?
                            .check_timeout_ms,
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
                    mode: delivery_mode_from(dialog.get(MODE)).into(),
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
/// Provider/model and effort fields for every model role.
pub(crate) fn model_fields(values: &Defaults) -> Vec<Field> {
    MODEL_ROLES
        .iter()
        .zip(values)
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
        .collect()
}
pub(crate) fn models_from(dialog: &Dialog) -> Result<Defaults, String> {
    let mut values: Defaults = std::array::from_fn(|_| None);
    for (role, value) in MODEL_ROLES.iter().zip(values.iter_mut()) {
        *value = ModelConfig::from_fields(
            dialog.get(&model_label(role)),
            dialog.get(&effort_label(role)),
        )?;
    }
    Ok(values)
}
fn switch_label(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}
fn delivery_mode_from(label: &str) -> &'static str {
    match label {
        "Direct push" => "direct",
        "Pull request" => "pr",
        _ => "local",
    }
}
fn delivery_label(mode: &str) -> &'static str {
    match mode {
        "direct" => "Direct push",
        "pr" => "Pull request",
        _ => "Local",
    }
}
/// Each setting with the project's value, and the default where it would change.
fn preview_text(rows: &[DefaultsRow]) -> String {
    rows.iter()
        .map(|row| {
            if row.changes {
                format!("{}: {} -> {}", row.setting, row.current, row.default)
            } else {
                format!("{}: {} (unchanged)", row.setting, row.current)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn check_label(check: &ProjectCheck) -> String {
    format!(
        "{} [{}]",
        check.name,
        if check.enabled { "enabled" } else { "disabled" }
    )
}

fn install_plan_text(plan: &Plan) -> String {
    let mut text = format!(
        "Recipe: {}\nSource: {}\nSource version: {}\nRepository: {}\nDestination: {}\n\nPackages:\n",
        plan.recipe,
        plan.source.display(),
        plan.source_version,
        plan.repository,
        plan.destination.display(),
    );
    for (package, revision) in plan.packages.iter().zip(&plan.package_revisions) {
        text.push_str(&format!("  {package} @ {revision}\n"));
    }
    text.push_str(&format!(
        "\nDownload cache: {} bytes / {} entries\nStage capacity: {} bytes\nRetained capacity: {} bytes\nFree-space reserve: {} bytes\nTime limit: {} seconds\nPolicy: {}\nLicense consent: {}\n",
        plan.download_cache_max_bytes,
        plan.download_cache_max_entries,
        plan.stage_max_bytes,
        plan.retained_max_bytes,
        plan.min_free_bytes,
        plan.timeout_seconds,
        if plan.enabled { "automatic enabled" } else { "manual" },
        if plan.consented { "recorded by a human" } else { "missing" },
    ));
    if !plan.issues.is_empty() {
        text.push_str("\nBefore install:\n");
        for issue in &plan.issues {
            text.push_str(&format!("  • {issue}\n"));
        }
    }
    text
}

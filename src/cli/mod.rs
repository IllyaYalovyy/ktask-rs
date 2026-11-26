mod agent;
mod args;
mod category;
mod evidence;
mod execute;
mod lens;
mod mcp;
mod output;
mod pause_all;
mod profile;

use crate::{
    application::{self, Service},
    instance::Instance,
    storage::{self, Database},
};
use args::{Cli, Command};
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

/// The status a shell reports for a process ended by SIGPIPE.
const SIGPIPE_EXIT: u8 = 141;

pub(super) struct Failure {
    pub status: u8,
    pub code: &'static str,
    pub message: String,
}
impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            status: 2,
            code: "usage",
            message: message.into(),
        }
    }
    /// The reader of stdout went away (`| head`): like other tools, stop without a word.
    fn closed_output() -> Self {
        Self {
            status: SIGPIPE_EXIT,
            code: "closed_output",
            message: String::new(),
        }
    }
    fn missing(message: impl Into<String>) -> Self {
        Self {
            status: 3,
            code: "not_found",
            message: message.into(),
        }
    }
}
impl From<storage::Error> for Failure {
    fn from(error: storage::Error) -> Self {
        let (status, code) = match &error {
            storage::Error::Conflict(_) | storage::Error::Breach(_) => (4, "conflict"),
            storage::Error::Incompatible(_) => (1, "incompatible_database"),
            storage::Error::Io(_) => (1, "io"),
            storage::Error::Sql(_) => (1, "database"),
        };
        // The code already names a conflict; its message is the reason alone.
        let message = match error {
            storage::Error::Conflict(message) => message,
            storage::Error::Breach(breach) => breach.reason,
            other => other.to_string(),
        };
        Self {
            status,
            code,
            message,
        }
    }
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self {
            status: 1,
            code: "io",
            message: error.to_string(),
        }
    }
}
type Result<T> = std::result::Result<T, Failure>;

pub fn launch(instance: Instance) -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let requested_json = arguments
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    let command = Cli::command()
        .name(crate::PRODUCT)
        .bin_name(instance.name())
        .version(instance.version());
    let matches = match command.try_get_matches_from(arguments) {
        Ok(matches) => matches,
        Err(error) if !error.use_stderr() => {
            return match error.print() {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    report(Failure::closed_output(), requested_json)
                }
                Err(error) => report(error.into(), requested_json),
            };
        }
        Err(error) => return report(Failure::usage(error.to_string()), requested_json),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return report(Failure::usage(error.to_string()), requested_json),
    };
    match run(instance, &cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report(error, cli.json),
    }
}

fn report(error: Failure, json_output: bool) -> ExitCode {
    if error.code == "closed_output" {
        return ExitCode::from(error.status);
    }
    let mut stderr = io::stderr().lock();
    // Never panic again if the error stream is itself closed.
    if json_output {
        let _ = writeln!(
            stderr,
            "{}",
            json!({"schema_version":1,"ok":false,"error":{"code":error.code,"message":error.message}})
        );
    } else {
        let _ = writeln!(
            stderr,
            "{}: {}",
            error.code,
            output::safe(&error.message).replace("\\n", "\n")
        );
    }
    ExitCode::from(error.status)
}
fn emit(reply: output::Reply, json_output: bool) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let written = if json_output {
        writeln!(
            stdout,
            "{}",
            json!({"schema_version":1,"ok":true,"data":reply.data})
        )
    } else {
        write!(stdout, "{}", reply.text)
    }
    .and_then(|()| stdout.flush());
    written.map_err(|error| match error.kind() {
        io::ErrorKind::BrokenPipe => Failure::closed_output(),
        _ => error.into(),
    })
}

/// `ktask projects`: every registered project, without selecting or registering one.
/// Every project of the instance; an instance without a database has none.
fn projects(paths: &crate::instance::Paths) -> Result<output::Reply> {
    let listed = match Database::open_existing(paths)? {
        Some(db) => crate::runner::registration::listed(paths, &db)?,
        None => Vec::new(),
    };
    let mut text = String::new();
    let mut data = Vec::new();
    for (project, alive) in &listed {
        let attention = project.attention(*alive).name();
        let current = project.current.as_ref();
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            project.id,
            attention,
            project.queued,
            output::safe(&project.name),
            output::safe(&project.root.to_string_lossy()),
            current.map_or("-".into(), |task| format!(
                "#{} {} · {}",
                task.id,
                task.state.label(),
                output::safe(&task.title)
            )),
        ));
        data.push(json!({
            "id": project.id,
            "name": project.name,
            "root": project.root.to_string_lossy(),
            "state": attention,
            "runner_alive": alive,
            "queued": project.queued,
            "finished": project.finished,
            "current": current.map(|task| json!({
                "id": task.id,
                "title": task.title,
                "kind": task.kind.as_str(),
                "state": task.state,
            })),
        }));
    }
    if listed.is_empty() {
        text.push_str("No projects.\n");
    }
    Ok(output::Reply {
        data: json!({"projects": data, "count": listed.len()}),
        text,
    })
}

/// The instance database for a project command that must not create storage.
fn registry_database(paths: &crate::instance::Paths, id: i64) -> Result<Database> {
    Database::open_existing(paths)?.ok_or_else(|| {
        crate::storage::Error::Conflict(format!("Project {id} is not registered")).into()
    })
}

fn project_move(instance: Instance, id: i64, folder: &std::path::Path) -> Result<output::Reply> {
    let paths = instance.paths()?;
    let mut db = registry_database(&paths, id)?;
    let folder = std::path::absolute(folder)?;
    let root = crate::application::registry::relocate(&mut db, &paths, id, &folder)?;
    Ok(output::Reply {
        text: format!("Project {id} moved to {}\n", root.display()),
        data: json!({"id": id, "root": root.to_string_lossy()}),
    })
}

fn project_delete(instance: Instance, id: i64, confirm: &str) -> Result<output::Reply> {
    let paths = instance.paths()?;
    let mut db = registry_database(&paths, id)?;
    let removed = crate::application::registry::remove(&mut db, &paths, id, confirm)?;
    let mut text = format!(
        "Deleted project {} ({}): {} tasks, {} attempts. {} was not touched.\n",
        removed.id,
        output::safe(&removed.name),
        removed.tasks,
        removed.attempts,
        removed.root.display()
    );
    for leftover in &removed.leftovers {
        text.push_str(&format!("not removed: {}\n", output::safe(leftover)));
    }
    Ok(output::Reply {
        text,
        data: json!({
            "id": removed.id,
            "name": removed.name,
            "root": removed.root.to_string_lossy(),
            "tasks": removed.tasks,
            "attempts": removed.attempts,
            "leftovers": removed.leftovers,
        }),
    })
}

/// The host-wide MCP server: it opens nothing and registers nothing before a tool call, and a
/// given `--project` only pins it to that registered project.
fn mcp_host(instance: Instance, cli: &Cli) -> Result<mcp::Host> {
    let paths = instance.paths()?;
    let pin = match &cli.project {
        None => None,
        Some(folder) => {
            let unregistered = || {
                Failure::missing("--project is not a registered project; use add first, or omit it")
            };
            let db = Database::open_existing(&paths)?.ok_or_else(unregistered)?;
            let service = Service::select(db, application::Selector::Folder(folder))?
                .ok_or_else(unregistered)?;
            Some(service.project_id)
        }
    };
    Ok(mcp::Host {
        paths,
        version: instance.version(),
        pin,
        directory: std::env::current_dir().ok(),
        actor: application::Actor {
            kind: application::ActorKind::Ai,
            name: cli.actor.clone().unwrap_or_else(|| "mcp".into()),
        },
    })
}

fn run(instance: Instance, cli: &Cli) -> Result<()> {
    if let Some(Command::Evidence {
        file,
        offset,
        limit,
        search,
    }) = &cli.command
    {
        let data = evidence::read(file, *offset, *limit as usize, search.as_deref())?;
        return emit(
            output::Reply {
                text: format!("{}\n", data),
                data,
            },
            cli.json,
        );
    }
    validate(cli)?;
    if cli.paths {
        let paths = instance.paths()?;
        if cli.json {
            for path in [
                &paths.executable,
                &paths.state,
                &paths.config,
                &paths.cache,
                &paths.data,
                &paths.runtime,
            ] {
                if path.to_str().is_none() {
                    return Err(Failure::usage(
                        "--json requires UTF-8 paths; use --paths without --json for native paths",
                    ));
                }
            }
            return emit(
                output::Reply {
                    data: json!({"instance":instance.name(),"executable":paths.executable,"state":paths.state,"config":paths.config,"cache":paths.cache,"data":paths.data,"runtime":paths.runtime}),
                    text: String::new(),
                },
                true,
            );
        }
        return emit(
            output::Reply {
                data: json!(null),
                text: paths.text(instance),
            },
            false,
        );
    }
    match cli.command {
        Some(Command::Pause { all: true }) => return emit(pause_all::run(false)?, cli.json),
        Some(Command::Shutdown) => return emit(pause_all::run(true)?, cli.json),
        Some(Command::Projects) => return emit(projects(&instance.paths()?)?, cli.json),
        Some(Command::ProjectMove { id, ref folder }) => {
            return emit(project_move(instance, id, folder)?, cli.json);
        }
        Some(Command::ProjectDelete { id, ref confirm }) => {
            return emit(project_delete(instance, id, confirm)?, cli.json);
        }
        Some(Command::Mcp) => return mcp::serve(mcp_host(instance, cli)?).map_err(Into::into),
        _ => {}
    }
    let root = cli.project.clone().map_or_else(std::env::current_dir, Ok)?;
    let Some(command) = &cli.command else {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(Failure::usage(
                "The TUI needs a terminal; use list, current, or --help for CLI commands",
            ));
        }
        let explicit = cli.project.is_some() || cli.project_name.is_some();
        crate::launch_tui(
            instance,
            &root,
            cli.project_name.as_deref(),
            explicit,
            cli.demo,
        )?;
        return Ok(());
    };
    if let Command::FollowUp {
        title,
        goal,
        criterion,
    } = command
    {
        let id = crate::runner::callback::file(
            cli.worker_token.as_deref().unwrap_or_default(),
            title,
            goal,
            criterion,
        )?;
        return emit(
            output::Reply {
                data: json!({"action":"created","tasks":[{"id":id,"version":1}]}),
                text: format!("created follow-up draft: #{id} v1\n"),
            },
            cli.json,
        );
    }
    if let Command::DependencyRequest { requirement } = command {
        let id = crate::runner::callback::dependency(
            cli.worker_token.as_deref().unwrap_or_default(),
            &requirement.requirement(),
        )?;
        return emit(
            output::Reply {
                data: json!({"action":"dependency_requested","id":id}),
                text: format!("dependency requested: #{id}\n"),
            },
            cli.json,
        );
    }
    if let Command::Cache { action } = command {
        let paths = instance.paths()?;
        return emit(execute::cache(&paths, &root, action.as_ref())?, cli.json);
    }
    // Preview is file validation only: no project registration or database initialization.
    if let Command::Import {
        file,
        preview: true,
        before,
    } = command
    {
        let definitions = application::transfer::import(file)?;
        return emit(
            output::Reply {
                data: json!({"preview":true,"before":before,"count":definitions.len(),"definitions":definitions.iter().map(output::definition).collect::<Vec<_>>()}),
                text: format!(
                    "Preview: {} tasks (no changes).\n{}",
                    definitions.len(),
                    definitions
                        .iter()
                        .map(|d| format!("{}\t{}\n", d.kind.as_str(), output::safe(&d.title)))
                        .collect::<String>()
                ),
            },
            cli.json,
        );
    }
    let paths = instance.paths()?;
    let creates_project = matches!(
        command,
        Command::Add(_)
            | Command::Import { preview: false, .. }
            | Command::CheckSet { .. }
            | Command::ModelDefault { .. }
            | Command::Defaults { .. }
            | Command::Profile { .. }
            | Command::Category { .. }
            | Command::Lens { .. }
            | Command::Init { .. }
            | Command::Tool {
                action: args::ToolAction::Register { .. }
            }
    );
    let mut service = if creates_project {
        // Validate the source before registering a project for an import that cannot succeed.
        if let Command::Import { file, .. } = command {
            application::transfer::import(file)?;
        }
        Service::open(Database::open(&paths)?, &root, cli.project_name.as_deref())?
    } else {
        let db = Database::open_existing(&paths)?.ok_or_else(|| {
            Failure::missing("No project database; use add to create the first task")
        })?;
        let service = Service::find(db, &root)?.ok_or_else(|| {
            Failure::missing("This directory is not a registered project; use add first")
        })?;
        if cli
            .project_name
            .as_ref()
            .is_some_and(|name| name != &service.project_name)
        {
            return Err(Failure::usage(
                "--project-name does not match the registered project",
            ));
        }
        service
    };
    let actor = application::Actor {
        kind: if matches!(cli.actor_kind, Some(args::ActorKind::Ai)) {
            application::ActorKind::Ai
        } else {
            application::ActorKind::Human
        },
        name: cli.actor.clone().unwrap_or_else(|| "owner".into()),
    };
    service.set_actor(actor.clone())?;
    if let Command::Agent { action } = command {
        return emit(agent::run(&mut service, &paths, actor, action)?, cli.json);
    }
    if let Command::Tool { action } = command {
        use args::ToolAction;
        let reply = match action {
            ToolAction::ProvisionPlan { recipe, source } => {
                let plan = service.provision_plan(&paths, recipe, *source)?;
                output::Reply {
                    text: format!(
                        "{}\n",
                        serde_json::to_string_pretty(&plan).expect("plan JSON")
                    ),
                    data: json!({"plan":plan}),
                }
            }
            ToolAction::ProvisionMode { mode } => {
                let enabled = matches!(mode, args::ProvisionMode::Automatic);
                service.set_provision_enabled(enabled)?;
                output::Reply {
                    text: format!(
                        "provisioning mode: {}\n",
                        if enabled { "automatic" } else { "manual" }
                    ),
                    data: json!({"mode":if enabled { "automatic" } else { "manual" }}),
                }
            }
            ToolAction::ProvisionConsent { recipe, source } => {
                service.consent_to_provision(recipe, *source)?;
                output::Reply {
                    text: format!("recorded human license consent for {recipe}\n"),
                    data: json!({"action":"consented","recipe":recipe}),
                }
            }
            ToolAction::ProvisionInstall { recipe, source } => {
                let waiting = service.provision_target()?;
                let id = service.provision_android(&paths, recipe, *source)?;
                let (rechecked, resumed) =
                    crate::runner::continue_after_provision(&mut service, &paths, waiting)?;
                output::Reply {
                    text: format!(
                        "published and bound Android SDK generation #{id}; dependency rechecked: {rechecked}; resumed: {resumed}\n"
                    ),
                    data: json!({"action":"provisioned","id":id,"rechecked":rechecked,"resumed":resumed}),
                }
            }
            ToolAction::ProvisionStatus => {
                let status = service.provision_status(&paths)?;
                output::Reply {
                    text: format!(
                        "{}\n",
                        serde_json::to_string_pretty(&status).expect("status JSON")
                    ),
                    data: json!({"status":status}),
                }
            }
            ToolAction::ProvisionCancel => {
                service.cancel_provision(&paths)?;
                output::Reply {
                    text: "provisioning cancellation requested\n".into(),
                    data: json!({"action":"cancellation_requested"}),
                }
            }
            ToolAction::List => {
                let tools = service.tools()?;
                output::Reply {
                    data: json!({"tools":tools}),
                    text: tools
                        .iter()
                        .map(|t| {
                            format!(
                                "#{} v{} {} {} {} {} {} {} {} {}\n",
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
                                    .map_or(String::new(), |v| format!("bound v{v}"))
                            )
                        })
                        .collect(),
                }
            }
            ToolAction::Register {
                name,
                version,
                platform,
                architecture,
                path,
                components,
                managed,
            } => {
                let id = service.register_tool(
                    &paths,
                    &application::tools::Registration {
                        name,
                        version,
                        platform,
                        architecture,
                        components,
                        path,
                        managed: *managed,
                    },
                )?;
                output::Reply {
                    data: json!({"action":"registered","id":id}),
                    text: format!("registered generation #{id}\n"),
                }
            }
            ToolAction::Bind {
                name,
                generation,
                if_version,
            } => {
                let version = service.bind_tool(name, *generation, *if_version)?;
                output::Reply {
                    data: json!({"action":"bound","name":name,"generation":generation,"version":version}),
                    text: format!("bound {name} to #{generation} v{version}\n"),
                }
            }
            ToolAction::Unbind { name, if_version } => {
                service.unbind_tool(name, *if_version)?;
                output::Reply {
                    data: json!({"action":"unbound","name":name}),
                    text: format!("unbound {name}\n"),
                }
            }
            ToolAction::Refresh {
                generation,
                version,
                components,
                if_version,
            } => {
                let row_version = service.refresh_external_tool(
                    &paths,
                    *generation,
                    *if_version,
                    version,
                    components,
                )?;
                output::Reply {
                    data: json!({"action":"refreshed","id":generation,"version":row_version}),
                    text: format!("refreshed generation #{generation} v{row_version}\n"),
                }
            }
        };
        return emit(reply, cli.json);
    }
    if service
        .pending_control()?
        .is_some_and(|control| !control.completed)
        && !crate::runner::supervisor_alive(&service)?
    {
        crate::runner::control::finish_pending(&mut service)?;
    }
    if let Command::Restart { target } = command {
        let current = service
            .current_task()?
            .ok_or_else(|| Failure::missing("Queue is empty"))?;
        if current.id != target.id {
            return Err(storage::Error::Conflict(format!(
                "Task #{} is current; restart it before task #{}",
                current.id, target.id
            ))
            .into());
        }
        if target
            .if_version
            .is_some_and(|version| version != current.version)
        {
            return Err(storage::Error::Conflict(format!(
                "Task #{} is version {}; read it again before restarting",
                current.id, current.version
            ))
            .into());
        }
        let saved = crate::runner::control::restart(
            &mut service,
            application::VersionedTask {
                id: current.id,
                version: current.version,
            },
            "",
        )?;
        let message = crate::runner::run(
            &mut service,
            &paths,
            &crate::runner::Options {
                queue: false,
                notify: false,
                continue_session: false,
            },
        )?;
        return emit(
            output::Reply {
                data: json!({"action":"restarted","id":current.id,"stash":saved.stash,"message":message}),
                text: format!("restarted: #{}\n{message}\n", current.id),
            },
            cli.json,
        );
    }
    if let Command::Run {
        queue,
        one,
        background,
        notify,
        continue_session,
    } = command
    {
        if *notify {
            // A detached supervisor leaves its launcher's session: with no controlling
            // terminal, closing that terminal or the TUI cannot signal it.
            nix::unistd::setsid().map_err(std::io::Error::from)?;
        }
        let resuming = service
            .current_task()?
            .is_some_and(|task| matches!(task.status.as_str(), "paused" | "waiting"));
        let queue = *queue
            || (!*one
                && if resuming {
                    service.execution_session()?.map_or(
                        service.preferences(&paths.config)?.continuation
                            == application::preferences::Continuation::Queue,
                        |session| session.mode == "queue",
                    )
                } else {
                    service.preferences(&paths.config)?.continuation
                        == application::preferences::Continuation::Queue
                });
        let message = if *background {
            crate::runner::background(&mut service, &paths, queue, *continue_session)?;
            "Runner started; inspect Current in the TUI".to_owned()
        } else {
            crate::runner::run(
                &mut service,
                &paths,
                &crate::runner::Options {
                    queue,
                    notify: *notify,
                    continue_session: *continue_session,
                },
            )?
        };
        if *notify {
            // A detached runner has no caller to answer: its outcome goes to its log.
            note!("{message}");
            return Ok(());
        }
        return emit(
            output::Reply {
                data: json!({"message":message}),
                text: format!("{message}\n"),
            },
            cli.json,
        );
    }
    if let Command::Breakdown { target, apply } = command {
        let task = service
            .task(target.id)?
            .ok_or_else(|| Failure::missing("Task not found"))?;
        if target
            .if_version
            .is_some_and(|version| version != task.version)
        {
            return Err(storage::Error::Conflict(format!(
                "Task #{} changed; read it again before retrying",
                target.id
            ))
            .into());
        }
        // Breaking a task down supersedes it, so only Pending work qualifies.
        task.require(application::lifecycle::Action::Supersede)?;
        let model = service.project_defaults()?[0]
            .clone()
            .ok_or_else(|| storage::Error::Conflict("Configure a planning model first".into()))?;
        service.validate_model("planning", &model)?;
        let definition = &task.definition().definition;
        let proposal = crate::runner::agent::propose_breakdown(
            crate::runner::process::stopped()?.as_ref(),
            &model,
            &definition.title,
            &definition.goal,
            &definition.criteria,
            &paths,
            &root,
        )?;
        let mut receipts = Vec::new();
        if *apply {
            let definitions = crate::runner::agent::breakdown_definitions(
                &proposal,
                service.defaults()?,
                &task.definition().definition,
            );
            let ready = service.preferences(&paths.config)?.planner_approval
                == application::preferences::PlannerApproval::Automatic;
            receipts = service.apply_breakdown(
                application::VersionedTask {
                    id: task.id,
                    version: task.version,
                },
                &definitions,
                &proposal.reason,
                ready,
            )?;
        }
        let data = json!({"proposal":proposal,"applied":apply,"receipts":receipts});
        return emit(
            output::Reply {
                text: format!("{}\n", serde_json::to_string(&data).unwrap()),
                data,
            },
            cli.json,
        );
    }
    if let Command::Retention {
        days,
        max_bytes,
        max_files,
        if_version,
    } = command
    {
        let expected = service.retention_policy()?;
        if if_version.is_some_and(|version| version != expected.version) {
            return Err(storage::Error::Conflict(
                "Retention version changed; read it again".into(),
            )
            .into());
        }
        let saved = service.set_storage_policy(
            expected,
            *days,
            max_bytes.unwrap_or(expected.max_bytes),
            max_files.unwrap_or(expected.max_files),
        )?;
        return emit(
            output::Reply {
                data: json!({"retention":saved}),
                text: format!(
                    "Retention: {} days, {} bytes, {} files (v{})\n",
                    saved.days, saved.max_bytes, saved.max_files, saved.version
                ),
            },
            cli.json,
        );
    }
    if let Command::Storage { apply } = command {
        let preview = service.storage_preview()?;
        let cleanup = if *apply {
            Some(service.cleanup_storage(&preview)?)
        } else {
            None
        };
        let mut text = format!(
            "Storage: {} logical bytes retained; {} bytes eligible across {} attempt(s){}\nAllocation: {} bytes in {} files/directories (SQLite {}, WAL {}, runner logs {}, evidence {}, directories {}); {} eligible file bytes, {} reusable SQLite bytes, {} protected bytes ({} younger than the {}-day minimum age, {} referenced by reused checks, {} open work). Limits: {} bytes, {} files{}{}{}\n{}",
            preview.usage_bytes,
            preview.reclaimable_bytes,
            preview.candidates.len(),
            if preview.deferred_for_live_work {
                " (deferred: live work)"
            } else {
                ""
            },
            preview.allocated_bytes,
            preview.allocated_files,
            preview.database_bytes,
            preview.wal_bytes,
            preview.runner_log_bytes,
            preview.evidence_bytes,
            preview.directory_entries,
            preview.eligible_file_bytes,
            preview.reusable_database_bytes,
            preview.protected_bytes,
            preview.protected_by_age_bytes,
            preview.policy.days,
            preview.protected_by_reference_bytes,
            preview.protected_open_bytes,
            preview.policy.max_bytes,
            preview.policy.max_files,
            if preview.effective_max_bytes != preview.policy.max_bytes || preview.effective_max_files != preview.policy.max_files {
                format!("; shared effective {} bytes, {} files", preview.effective_max_bytes, preview.effective_max_files)
            } else { String::new() },
            if preview.launch_capacity_reached { " · WORK PAUSED AT CAPACITY RESERVE" } else { "" },
            preview.vacuum_note.as_ref().map(|n| format!("\nNote: {n}")).unwrap_or_default(),
            preview
                .candidates
                .iter()
                .map(|c| format!(
                    "task #{} attempt #{}: {} output bytes, {} artifact bytes ({} files; {} pending cleanup)\n",
                    c.task_id, c.attempt_id, c.output_bytes, c.artifact_bytes, c.artifacts, c.pending_artifacts
                ))
                .collect::<String>()
        );
        if let Some(receipt) = &cleanup {
            text.push_str(&format!(
                "Processed {} attempt(s), {} bytes{}\n",
                receipt.attempts,
                receipt.bytes,
                if receipt.batch_limit_reached {
                    "; batch limit reached, repeat to continue"
                } else {
                    ""
                }
            ));
        }
        return emit(
            output::Reply {
                data: json!({"applied":apply,"preview":preview,"cleanup":cleanup}),
                text,
            },
            cli.json,
        );
    }
    let mut reply = execute::command(&mut service, &paths.config, &paths.cache, &paths, command)?;
    if matches!(
        command,
        Command::Answer { .. } | Command::Approve { .. } | Command::Done { .. }
    ) {
        // The edit is committed. Report any continuation failure alongside its
        // successful receipt, and retain the reason in the execution session.
        if let Err(error) = crate::runner::continue_after_intervention(&mut service, &paths) {
            let message = error.to_string();
            reply.data["continuation_error"] = json!(message);
            reply
                .text
                .push_str(&format!("Continuation could not be confirmed: {message}\n"));
        }
    }
    emit(reply, cli.json)
}

fn validate(cli: &Cli) -> Result<()> {
    if std::env::var_os("KTASK_WORKER_CALLBACK").is_some()
        && !matches!(
            cli.command,
            Some(Command::FollowUp { .. } | Command::DependencyRequest { .. })
        )
    {
        return Err(Failure::usage(
            "Workers may only request follow-up drafts or missing dependencies",
        ));
    }
    if cli.worker_token.is_some()
        != matches!(
            cli.command,
            Some(Command::FollowUp { .. } | Command::DependencyRequest { .. })
        )
    {
        return Err(Failure::usage(
            "Follow-up and dependency-request callbacks require --worker-token and a live worker",
        ));
    }
    if cli.worker_token.is_some()
        && (cli.actor.is_some()
            || matches!(cli.actor_kind, Some(args::ActorKind::Ai))
            || cli.project_name.is_some()
            || cli.paths
            || cli.demo)
    {
        return Err(Failure::usage(
            "Worker callbacks cannot set actor or instance options",
        ));
    }
    if matches!(cli.command, Some(Command::Run { notify: true, .. })) && cli.json {
        return Err(Failure::usage(
            "--notify is an internal startup handshake, not JSON output",
        ));
    }
    if cli.json
        && let Some(Command::Import { file, .. } | Command::Export { file, .. }) = &cli.command
        && file.to_str().is_none()
    {
        return Err(Failure::usage("--json requires a UTF-8 import/export path"));
    }
    if matches!(
        cli.command,
        Some(
            Command::Shutdown
                | Command::Projects
                | Command::ProjectMove { .. }
                | Command::ProjectDelete { .. }
        )
    ) && (cli.project.is_some() || cli.project_name.is_some())
    {
        return Err(Failure::usage(
            "shutdown, projects, project-move and project-delete take no --project; name the project by ID",
        ));
    }
    if (cli.paths || cli.demo) && cli.command.is_some() {
        return Err(Failure::usage(
            "--paths and --demo cannot be combined with task commands",
        ));
    }
    if cli.demo
        && (cli.json
            || cli.project.is_some()
            || cli.project_name.is_some()
            || cli.actor.is_some()
            || matches!(cli.actor_kind, Some(args::ActorKind::Ai)))
    {
        return Err(Failure::usage(
            "--demo opens only the isolated simulator; do not combine it with project/actor/JSON options",
        ));
    }
    if cli.command.is_none()
        && !cli.paths
        && (cli.json || cli.actor.is_some() || matches!(cli.actor_kind, Some(args::ActorKind::Ai)))
    {
        return Err(Failure::usage(
            "JSON and actor options require a task command",
        ));
    }
    if matches!(cli.command, Some(Command::Mcp)) {
        if cli.project.as_deref().is_some_and(|p| !p.is_absolute()) {
            return Err(Failure::usage(
                "mcp --project must be an absolute registered path",
            ));
        }
        if cli.project_name.is_some() {
            return Err(Failure::usage(
                "mcp registers no project; omit --project-name",
            ));
        }
        if matches!(cli.actor_kind, Some(args::ActorKind::Human)) {
            return Err(Failure::usage("mcp always records the actor as AI"));
        }
    } else if matches!(cli.actor_kind, Some(args::ActorKind::Ai)) && cli.actor.is_none() {
        return Err(Failure::usage("--actor-kind ai requires --actor NAME"));
    }
    if cli.actor.as_ref().is_some_and(|s| s.trim().is_empty())
        || cli
            .project_name
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
    {
        return Err(Failure::usage("Actor and project names must not be blank"));
    }
    match &cli.command {
        Some(Command::Add(add)) => add.settings.validate()?,
        Some(Command::Edit { changes, .. }) => {
            changes.settings.validate()?;
            if changes.title.is_none()
                && changes.goal.is_none()
                && changes.kind.is_none()
                && changes.category.is_none()
                && changes.lenses.selection().is_none()
                && changes.criterion.is_empty()
                && changes.approval.is_none()
                && changes.settings.empty()
                && !changes.apply_project_defaults
                && !changes.preview_project_defaults
            {
                return Err(Failure::usage("edit requires at least one changed field"));
            }
        }
        _ => {}
    }
    Ok(())
}

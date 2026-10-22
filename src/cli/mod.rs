mod args;
mod execute;
mod output;

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
            storage::Error::Conflict(_) => (4, "conflict"),
            storage::Error::Incompatible(_) => (1, "incompatible_database"),
            storage::Error::Io(_) => (1, "io"),
            storage::Error::Sql(_) => (1, "database"),
        };
        Self {
            status,
            code,
            message: error.to_string(),
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
    if json_output {
        writeln!(
            stdout,
            "{}",
            json!({"schema_version":1,"ok":true,"data":reply.data})
        )?;
    } else {
        write!(stdout, "{}", reply.text)?;
    }
    stdout.flush()?;
    Ok(())
}

fn run(instance: Instance, cli: &Cli) -> Result<()> {
    validate(cli)?;
    if cli.paths {
        let paths = instance.paths()?;
        if cli.json {
            for path in [
                &paths.executable,
                &paths.state,
                &paths.config,
                &paths.cache,
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
                    data: json!({"instance":instance.name(),"executable":paths.executable,"state":paths.state,"config":paths.config,"cache":paths.cache,"runtime":paths.runtime}),
                    text: String::new(),
                },
                true,
            );
        }
        paths.print(instance)?;
        return Ok(());
    }
    let root = cli.project.clone().map_or_else(std::env::current_dir, Ok)?;
    let Some(command) = &cli.command else {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(Failure::usage(
                "The TUI needs a terminal; use list, current, or --help for CLI commands",
            ));
        }
        crate::launch_tui(instance, &root, cli.project_name.as_deref(), cli.demo)?;
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
    service.set_actor(application::Actor {
        kind: match cli.actor_kind {
            args::ActorKind::Human => application::ActorKind::Human,
            args::ActorKind::Ai => application::ActorKind::Ai,
        },
        name: cli.actor.clone().unwrap_or_else(|| "owner".into()),
    })?;
    if let Command::Run {
        queue,
        one,
        background,
        notify,
        continue_session,
    } = command
    {
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
                    recover: false,
                    confirm_worker_cleanup: false,
                    recover_target: None,
                },
            )?
        };
        return emit(
            output::Reply {
                data: json!({"message":message}),
                text: format!("{message}\n"),
            },
            cli.json,
        );
    }
    if let Command::Recover {
        confirm_worker_cleanup,
    } = command
    {
        let message = crate::runner::run(
            &mut service,
            &paths,
            &crate::runner::Options {
                recover: true,
                confirm_worker_cleanup: *confirm_worker_cleanup,
                ..Default::default()
            },
        )?;
        return emit(
            output::Reply {
                data: json!({"message":message}),
                text: format!("{message}\n"),
            },
            cli.json,
        );
    }
    if let Command::Remediate { id, apply } = command {
        let current = service.current_task()?.ok_or_else(|| Failure {
            status: 4,
            code: "conflict",
            message: "No failed current task".into(),
        })?;
        if current.id != *id || current.status != "failed" {
            return Err(Failure {
                status: 4,
                code: "conflict",
                message: "Remediate requires the failed current task".into(),
            });
        }
        let task = service
            .task(*id)?
            .ok_or_else(|| Failure::missing("Task not found"))?;
        let definition = task.definition().definition.clone();
        let model = definition.models[3].as_ref().ok_or_else(|| Failure {
            status: 4,
            code: "conflict",
            message: "Configure a remediation model first".into(),
        })?;
        let pin = service.begin_remediation_evidence(application::VersionedTask {
            id: current.id,
            version: current.version,
        })?;
        let evidence = service.remediation_evidence(*id, &paths.state)?;
        let judgment = crate::runner::agent::propose_remediation(
            crate::runner::process::stopped()?.as_ref(),
            model,
            &definition,
            &evidence,
            &paths,
            &root,
            pin,
        )?;
        let mut receipts = Vec::new();
        if *apply {
            let ready = service.preferences(&paths.config)?.planner_approval
                == application::preferences::PlannerApproval::Automatic;
            receipts = crate::runner::remediation::apply_judgment(
                &mut service,
                application::VersionedTask {
                    id: current.id,
                    version: current.version,
                },
                &judgment,
                ready,
            )?;
        }
        let data = json!({"judgment":judgment,"applied":apply,"receipts":receipts});
        return emit(
            output::Reply {
                text: format!("{}\n", serde_json::to_string(&data).unwrap()),
                data,
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
        let current_id = service.current_task()?.map(|t| t.id);
        if !matches!(task.status.as_str(), "new" | "ready")
            && !(task.status == "failed" && current_id == Some(task.id))
        {
            return Err(storage::Error::Conflict(
                "Only pending work or failed current can be broken down".into(),
            )
            .into());
        }
        let model = service.project_defaults()?[0]
            .clone()
            .ok_or_else(|| storage::Error::Conflict("Configure a planning model first".into()))?;
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
            let definitions =
                crate::runner::agent::breakdown_definitions(&proposal, service.defaults()?);
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
    if let Command::Retention { days, if_version } = command {
        let expected = service.retention_policy()?;
        if if_version.is_some_and(|version| version != expected.version) {
            return Err(storage::Error::Conflict(
                "Retention version changed; read it again".into(),
            )
            .into());
        }
        let saved = service.set_retention(expected, *days)?;
        return emit(
            output::Reply {
                data: json!({"retention":saved}),
                text: format!("Retention: {} days (v{})\n", saved.days, saved.version),
            },
            cli.json,
        );
    }
    if let Command::Storage { apply } = command {
        let preview = service.storage_preview()?;
        if *apply {
            service.cleanup_storage(&preview)?;
        }
        let text = format!(
            "Storage: {} bytes retained; {} bytes eligible across {} attempt(s){}\n{}",
            preview.usage_bytes,
            preview.reclaimable_bytes,
            preview.candidates.len(),
            if preview.deferred_for_live_work {
                " (deferred: live work)"
            } else {
                ""
            },
            preview
                .candidates
                .iter()
                .map(|c| format!(
                    "task #{} attempt #{}: {} output bytes, {} artifact bytes ({} files; {} pending cleanup)\n",
                    c.task_id, c.attempt_id, c.output_bytes, c.artifact_bytes, c.artifacts, c.pending_artifacts
                ))
                .collect::<String>()
        );
        return emit(
            output::Reply {
                data: json!({"applied":apply,"preview":preview}),
                text,
            },
            cli.json,
        );
    }
    let mut reply = execute::command(&mut service, command)?;
    if matches!(
        command,
        Command::Answer { .. }
            | Command::Approve { .. }
            | Command::Ack { .. }
            | Command::Resolve { .. }
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
        && !matches!(cli.command, Some(Command::FollowUp { .. }))
    {
        return Err(Failure::usage("Workers may only file follow-up drafts"));
    }
    if cli.worker_token.is_some() != matches!(cli.command, Some(Command::FollowUp { .. })) {
        return Err(Failure::usage(
            "Worker callbacks may only file follow-up drafts; follow-up requires --worker-token",
        ));
    }
    if cli.worker_token.is_some()
        && (cli.actor.is_some()
            || !matches!(cli.actor_kind, args::ActorKind::Human)
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
            || matches!(cli.actor_kind, args::ActorKind::Ai))
    {
        return Err(Failure::usage(
            "--demo opens only the isolated simulator; do not combine it with project/actor/JSON options",
        ));
    }
    if cli.command.is_none()
        && !cli.paths
        && (cli.json || cli.actor.is_some() || matches!(cli.actor_kind, args::ActorKind::Ai))
    {
        return Err(Failure::usage(
            "JSON and actor options require a task command",
        ));
    }
    if matches!(cli.actor_kind, args::ActorKind::Ai) && cli.actor.is_none() {
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
                && changes.criterion.is_empty()
                && changes.approval.is_none()
                && changes.settings.empty()
            {
                return Err(Failure::usage("edit requires at least one changed field"));
            }
        }
        _ => {}
    }
    Ok(())
}

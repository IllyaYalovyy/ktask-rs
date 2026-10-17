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
        .name(instance.name())
        .bin_name(instance.name());
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
        background,
        notify,
    } = command
    {
        let message = if *background {
            crate::runner::background(&mut service, &paths, *queue)?;
            "Runner started; inspect Current in the TUI".to_owned()
        } else {
            crate::runner::run(
                &mut service,
                &paths,
                &crate::runner::Options {
                    queue: *queue,
                    notify: *notify,
                    recover: false,
                    confirm_worker_cleanup: false,
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
    emit(execute::command(&mut service, command)?, cli.json)
}

fn validate(cli: &Cli) -> Result<()> {
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

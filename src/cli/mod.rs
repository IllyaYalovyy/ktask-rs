mod args;
mod execute;
mod output;

use crate::{
    application::{self, Service},
    instance::Instance,
    storage::{self, Database},
};
use args::{CacheAction, Cli, Command};
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
        // The code already names a conflict; its message is the reason alone.
        let message = match error {
            storage::Error::Conflict(message) => message,
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
    if let Command::Cache { action } = command {
        let paths = instance.paths()?;
        return emit(cache_reply(&paths, &root, action.as_ref())?, cli.json);
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
            | Command::Init { .. }
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
    if service
        .pending_control()?
        .is_some_and(|control| !control.completed)
        && !crate::runner::supervisor_alive(&service)?
    {
        crate::runner::control::finish_pending(&mut service)?;
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
    let mut reply = execute::command(&mut service, command)?;
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

fn cache_reply(
    paths: &crate::instance::Paths,
    project: &std::path::Path,
    action: Option<&CacheAction>,
) -> Result<output::Reply> {
    use crate::build_cache as cache;
    let bytes = |n: u64| format!("{n} bytes");
    match action {
        None => {
            let inv = cache::inventory(paths, project)?;
            let mut text = format!(
                "Managed (owned): {} in {} files · limit {} per cache, {} files · free {} of {}\n",
                bytes(inv.managed_bytes),
                inv.managed_files,
                bytes(inv.policy.max_bytes),
                inv.policy.max_files,
                bytes(inv.free_bytes),
                bytes(inv.total_bytes),
            );
            for (title, entries) in [
                ("Owned", &inv.owned),
                ("Not managed", &inv.unowned),
                ("Provider storage (never cleaned)", &inv.provider),
            ] {
                text.push_str(&format!("{title}:\n"));
                for e in entries {
                    text.push_str(&format!(
                        "  {} [{}/{}] {} in {} files — {}\n",
                        e.path.display(),
                        e.kind,
                        e.ownership,
                        bytes(e.bytes),
                        e.files,
                        output::safe(&e.note)
                    ));
                }
            }
            for line in &inv.pressure {
                text.push_str(&format!("Pressure: {line}\n"));
            }
            for line in &inv.exclusions {
                text.push_str(&format!("Excluded: {line}\n"));
            }
            Ok(output::Reply {
                data: json!({"cache":inv}),
                text,
            })
        }
        Some(CacheAction::Clean { apply }) => {
            let done = cache::clean(paths, *apply)?;
            let mut text = format!(
                "{} cleanup of {} owned cache(s); {} abandoned scratch run(s) reclaimed\n",
                if *apply { "Applied" } else { "Preview" },
                done.reports.len(),
                done.scratch_reclaimed
            );
            for r in &done.reports {
                text.push_str(&match &r.deferred {
                    Some(why) => format!("  {}: deferred, {why}\n", r.root.display()),
                    None => format!(
                        "  {}: {} generation(s) {}, {} freed, {} protected; {} → {}{}\n",
                        r.root.display(),
                        r.removed_generations,
                        if *apply {
                            "removed"
                        } else {
                            "would be removed"
                        },
                        bytes(r.freed_bytes),
                        r.protected_generations,
                        bytes(r.before_bytes),
                        bytes(r.after_bytes),
                        if r.over_capacity {
                            " · STILL OVER LIMIT (only warm or protected output remains)"
                        } else {
                            ""
                        },
                    ),
                });
                for note in &r.skipped {
                    text.push_str(&format!("    {note}\n"));
                }
            }
            Ok(output::Reply {
                data: json!({"applied":apply,"scratch_reclaimed":done.scratch_reclaimed,"caches":done.reports}),
                text,
            })
        }
        Some(CacheAction::Adopt { path }) => {
            let target = path.clone().unwrap_or_else(|| project.join("target"));
            let claim = cache::adopt(paths, project, &target)?;
            Ok(output::Reply {
                data: json!({"root":claim.root,"already_owned":claim.already_owned}),
                text: format!(
                    "{} {}\n",
                    if claim.already_owned {
                        "Already owned:"
                    } else {
                        "Now owned:"
                    },
                    claim.root.display()
                ),
            })
        }
        Some(CacheAction::Limits {
            max_bytes,
            max_files,
            keep_generations,
            grace_secs,
            min_free_bytes,
            if_version,
        }) => {
            let current = cache::policy(paths)?;
            let mut next = current;
            let edits = [
                max_bytes,
                max_files,
                keep_generations,
                grace_secs,
                min_free_bytes,
            ];
            if edits.iter().any(|e| e.is_some()) {
                if if_version.is_some_and(|v| v != current.version) {
                    return Err(storage::Error::Conflict(
                        "Build cache policy changed; read it again".into(),
                    )
                    .into());
                }
                next.max_bytes = max_bytes.unwrap_or(next.max_bytes);
                next.max_files = max_files.unwrap_or(next.max_files);
                next.keep_generations = keep_generations.unwrap_or(next.keep_generations);
                next.grace_secs = grace_secs.unwrap_or(next.grace_secs);
                next.min_free_bytes = min_free_bytes.unwrap_or(next.min_free_bytes);
                next = cache::set_policy(paths, current, next)?;
            }
            Ok(output::Reply {
                data: json!({"limits":next}),
                text: format!(
                    "Build cache limits (v{}): {} bytes, {} files per cache; keep {} generations; {}s grace; {} byte free-space floor\n",
                    next.version,
                    next.max_bytes,
                    next.max_files,
                    next.keep_generations,
                    next.grace_secs,
                    next.min_free_bytes
                ),
            })
        }
    }
}

//! `ktask-rs run`: run the pending tasks in queue order, one attempt each, until one stops
//! it.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

use std::path::Path;

use ktask_adapters::{
    FileAttemptOutput, FileInstructionFiles, FileRunLock, FileSessionLog, GitCli, ProcessCommands,
    RealSleep, SystemClock, builtin_providers, configured_provider, echo,
};
use ktask_core::{
    CHECK_STEP, COMMIT_STEP, HEALTH_CHECK_STEP, PUSH_STEP, REVIEW_STEP, RunContext, RunReport,
    RunRequest, SYNC_STEP, Settings, TEST_STEP, TaskProviders, effective_instructions_dir,
    effective_provider, effective_resolver_provider, effective_transport_retries, show_providers,
};

use crate::context::{
    current_exe, merge_project, open_journal, open_registry, outputs_dir_file, resolve,
    run_lock_file, sessions_dir_file,
};
use crate::error::Failure;
use crate::render;

/// The steps `settings` has switched off, named as [`RunContext`]'s `disabled_steps` expects.
fn disabled_steps(settings: &Settings) -> Vec<&'static str> {
    [
        (SYNC_STEP, settings.sync_step),
        (HEALTH_CHECK_STEP, settings.health_check_step),
        (CHECK_STEP, settings.check_step),
        (REVIEW_STEP, settings.review_step),
        (TEST_STEP, settings.testing_step),
        (COMMIT_STEP, settings.commit_step),
        (PUSH_STEP, settings.push_step),
    ]
    .into_iter()
    .filter(|(_, switch)| !ktask_core::step_enabled(*switch))
    .map(|(step, _)| step)
    .collect()
}

/// `ktask-rs run`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// How long an attempt may run before it is killed, in seconds [default: the project's
    /// attempt-timeout setting, or 14400 (four hours) when it has none]
    #[arg(long, value_name = "SECONDS")]
    attempt_timeout: Option<u64>,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Print the run's own report as JSON instead of text
    #[arg(long)]
    json: bool,
}

/// The model names this run uses: the implementation (and other agent steps') own model, and
/// the resolver's own — each empty when the project's settings name none.
#[derive(Clone, Copy)]
struct EffectiveModels<'a> {
    model: &'a str,
    resolver_model: &'a str,
}

/// Where this run keeps per-attempt state outside the project: sessions, for a provider that
/// supports resuming one, and captured output.
#[derive(Clone, Copy)]
struct StateDirs<'a> {
    sessions_dir: &'a Path,
    outputs_dir: &'a Path,
}

/// The [`RunContext`] `run` hands `run_queue`, built from `project`'s own directory and name,
/// `binary_path`, `args` and `settings`, and `settings`' own steps switched off, held in
/// `disabled` since `RunContext` only borrows them.
fn run_context<'a>(
    project: &'a ktask_core::Project,
    binary_path: &'a Path,
    args: &Args,
    settings: &'a Settings,
    disabled: &'a [&'static str],
    models: EffectiveModels<'a>,
    dirs: StateDirs<'a>,
) -> RunContext<'a> {
    RunContext {
        project_name: &project.name,
        project_dir: &project.path,
        binary_path,
        attempt_timeout: ktask_core::effective_attempt_timeout(settings, args.attempt_timeout),
        health_check_command: settings.health_check_command.as_deref(),
        check_command: settings.check_command.as_deref(),
        tracked_branch: settings.tracked_branch.as_deref(),
        disabled_steps: disabled,
        max_attempts: ktask_core::effective_max_attempts(settings),
        transport_retries: effective_transport_retries(settings),
        model: models.model,
        resolver_model: models.resolver_model,
        sessions_dir: dirs.sessions_dir,
        outputs_dir: dirs.outputs_dir,
        instructions_dir: effective_instructions_dir(settings),
    }
}

/// Builds one configured provider selected by this project setting.
fn configured_providers(
    settings: &Settings,
) -> Result<BTreeMap<String, ktask_core::Provider>, Failure> {
    let views =
        show_providers(settings, &builtin_providers()).map_err(|error| error.to_string())?;
    Ok(views
        .into_iter()
        .map(|provider| {
            let configured = if provider.name == echo::NAME {
                echo::provider()
            } else {
                configured_provider(&provider.name, &provider.definition)
            };
            (provider.name, configured)
        })
        .collect())
}

/// Runs with the configured provider catalogue after the project execution context is ready.
fn run_with_providers(
    journal: &ktask_adapters::SqliteJournal,
    settings: &Settings,
    lock: &FileRunLock,
    context: RunContext<'_>,
) -> Result<RunReport, Failure> {
    let providers = configured_providers(settings)?;
    let provider = providers
        .get(effective_provider(settings))
        .ok_or_else(|| format!("unknown provider {:?}", effective_provider(settings)))?;
    let resolver = providers
        .get(effective_resolver_provider(settings))
        .ok_or_else(|| {
            format!(
                "unknown provider {:?}",
                effective_resolver_provider(settings)
            )
        })?;
    Ok(ktask_core::run_queue(
        ktask_core::RunPorts {
            journal,
            clock: &SystemClock,
            commands: &ProcessCommands,
            git: &GitCli,
            session_log: &FileSessionLog,
            sleep: &RealSleep,
        },
        lock,
        RunRequest {
            providers: TaskProviders {
                default: provider,
                resolver,
                named: &providers,
            },
            context,
            output: &FileAttemptOutput::new(context.outputs_dir.to_path_buf()),
            instruction_files: &FileInstructionFiles,
        },
    )?)
}

/// Resolves the project and its settings, builds the context `ktask_core::run_queue` needs,
/// and runs it with the real adapters — [`run`]'s own work, pulled out of it so it stays
/// within the workspace's function-length limit.
fn execute(args: &Args, project: Option<&str>) -> Result<RunReport, Failure> {
    let registry = open_registry()?;
    let project = merge_project(project, args.project.as_deref())?;
    let (project, settings) = resolve(&registry, project.as_deref())?;
    let journal = open_journal(&project)?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    let binary_path = current_exe()?;
    let model = settings.model.clone().unwrap_or_default();
    let resolver_model = settings.resolver_model.clone().unwrap_or_default();
    let disabled = disabled_steps(&settings);
    let sessions_dir = sessions_dir_file(&project)?;
    let outputs_dir = outputs_dir_file(&project)?;
    let context = run_context(
        &project,
        &binary_path,
        args,
        &settings,
        &disabled,
        EffectiveModels {
            model: &model,
            resolver_model: &resolver_model,
        },
        StateDirs {
            sessions_dir: &sessions_dir,
            outputs_dir: &outputs_dir,
        },
    );
    run_with_providers(&journal, &settings, &lock, context)
}

/// Runs the pending tasks of the resolved project in queue order, one attempt each with the
/// `echo` provider, until one stops it; renders what happened and maps it to an exit code.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let report = execute(args, project)?;
    let stopped = render::run(&report, args.json, stdout)?;
    Ok(if stopped {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

//! `ktask-rs run`: run the pending tasks in queue order, one attempt each, until one stops
//! it.

use std::io::Write;
use std::process::ExitCode;

use std::path::Path;

use ktask_adapters::{FileRunLock, FileSessionLog, GitCli, ProcessCommands, SystemClock, echo};
use ktask_core::{
    COMMIT_STEP, HEALTH_CHECK_STEP, PUSH_STEP, REVIEW_STEP, RunContext, SYNC_STEP, Settings,
    TEST_STEP,
};

use crate::context::{
    current_exe, merge_project, open_journal, open_registry, resolve, run_lock_file,
    sessions_dir_file,
};
use crate::error::Failure;
use crate::render;

/// The steps `settings` has switched off, named as [`RunContext`]'s `disabled_steps` expects.
fn disabled_steps(settings: &Settings) -> Vec<&'static str> {
    [
        (SYNC_STEP, settings.sync_step),
        (HEALTH_CHECK_STEP, settings.health_check_step),
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

/// The [`RunContext`] `run` hands `run_queue`, built from `project`'s own directory and name,
/// `binary_path`, `args` and `settings`, and `settings`' own steps switched off, held in
/// `disabled` since `RunContext` only borrows them.
#[allow(clippy::too_many_arguments)]
fn run_context<'a>(
    project: &'a ktask_core::Project,
    binary_path: &'a Path,
    args: &Args,
    settings: &'a Settings,
    disabled: &'a [&'static str],
    resolver_model: &'a str,
    sessions_dir: &'a Path,
) -> RunContext<'a> {
    RunContext {
        project_name: &project.name,
        project_dir: &project.path,
        binary_path,
        attempt_timeout: ktask_core::effective_attempt_timeout(settings, args.attempt_timeout),
        health_check_command: settings.health_check_command.as_deref(),
        tracked_branch: settings.tracked_branch.as_deref(),
        disabled_steps: disabled,
        max_attempts: ktask_core::effective_max_attempts(settings),
        resolver_model,
        sessions_dir,
    }
}

/// Runs the pending tasks of the resolved project in queue order, one attempt each with the
/// `echo` provider, until one stops it; renders what happened and maps it to an exit code.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let project = merge_project(project, args.project.as_deref())?;
    let (project, settings) = resolve(&registry, project.as_deref())?;
    let journal = open_journal(&project)?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    let binary_path = current_exe()?;
    let resolver_model = settings.resolver_model.clone().unwrap_or_default();
    let disabled = disabled_steps(&settings);
    let sessions_dir = sessions_dir_file(&project)?;
    let context = run_context(
        &project,
        &binary_path,
        args,
        &settings,
        &disabled,
        &resolver_model,
        &sessions_dir,
    );
    let report = ktask_core::run_queue(
        &journal,
        &SystemClock,
        &ProcessCommands,
        &GitCli,
        &echo::PROVIDER,
        &FileSessionLog,
        &lock,
        context,
    )?;
    let stopped = render::run(&report, args.json, stdout)?;
    Ok(if stopped {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

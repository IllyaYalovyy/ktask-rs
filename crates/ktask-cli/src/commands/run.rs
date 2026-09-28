//! `ktask-rs run`: run the pending tasks in queue order, one attempt each, until one stops
//! it.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use ktask_adapters::{FileRunLock, ProcessCommands, SystemClock, echo};
use ktask_core::RunContext;

use crate::context::{current_exe, open_journal, open_registry, resolve, run_lock_file};
use crate::error::Failure;
use crate::render;

/// `run`'s default time limit for one attempt: four hours.
const DEFAULT_ATTEMPT_TIMEOUT_SECS: u64 = 14_400;

/// `ktask-rs run`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// How long an attempt may run before it is killed, in seconds
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_ATTEMPT_TIMEOUT_SECS)]
    attempt_timeout: u64,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Runs the pending tasks of the resolved project in queue order, one attempt each with the
/// `echo` provider, until one stops it; renders what happened and maps it to an exit code.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let project = resolve(&registry, args.project.as_deref())?;
    let journal = open_journal(&project)?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    let binary_path = current_exe()?;
    let report = ktask_core::run_queue(
        &journal,
        &SystemClock,
        &ProcessCommands,
        &echo::PROVIDER,
        &lock,
        RunContext {
            project_name: &project.name,
            project_dir: &project.path,
            binary_path: &binary_path,
            attempt_timeout: Duration::from_secs(args.attempt_timeout),
        },
    )?;
    let stopped = render::run(&report, stdout)?;
    Ok(if stopped {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

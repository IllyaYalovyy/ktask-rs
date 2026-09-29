//! `ktask-rs status`: what ran and how it ended.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{FileRunLock, SystemClock};

use crate::context::{merge_project, open_journal, open_registry, resolve, run_lock_file};
use crate::error::Failure;
use crate::render;

/// `ktask-rs status`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Print a JSON array instead of one line per task
    #[arg(long)]
    json: bool,
}

/// Prints every task that was attempted, with its most recent attempt. A task the journal
/// still calls `running` is shown `interrupted` at once when no run is alive to finish it,
/// rather than waiting for the next `run` to reconcile it.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let project = merge_project(project, args.project.as_deref())?;
    let (project, _settings) = resolve(&registry, project.as_deref())?;
    let journal = open_journal(&project)?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    let entries = ktask_core::status(&journal, &SystemClock, &lock).map_err(|e| e.to_string())?;
    render::status(&entries, args.json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

//! `ktask-rs status`: what ran and how it ended.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{FileAttemptOutput, FileRunLock, SystemClock};

use crate::context::{
    merge_project, open_journal, open_registry, outputs_dir_file, resolve, run_lock_file,
};
use crate::error::Failure;
use crate::render;

/// `ktask-rs status`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Print one JSON object — the run band at `run`, every task at `tasks` — instead of text
    #[arg(long)]
    json: bool,
}

/// Prints the run band first — what is running, where it most recently stopped and why, or
/// that the queue is idle — then every task that was attempted, with every attempt it has had.
/// A task the journal still calls `running` is shown `interrupted` at once when no run is
/// alive to finish it, rather than waiting for the next `run` to reconcile it.
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
    let output = FileAttemptOutput::new(outputs_dir_file(&project)?);
    let silent_after = ktask_core::effective_silent_after(&settings);
    let entries =
        ktask_core::status_with_output(&journal, &SystemClock, &lock, &output, silent_after)
            .map_err(|e| e.to_string())?;
    let band =
        ktask_core::run_band_with_output(&journal, &SystemClock, &lock, &output, silent_after)
            .map_err(|e| e.to_string())?;
    render::status(&band, &entries, args.json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

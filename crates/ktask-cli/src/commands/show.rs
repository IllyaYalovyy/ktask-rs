//! `ktask-rs show`: one task, in full — every field, every attempt, nothing elided.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{FileRunLock, SystemClock};
use ktask_core::TaskId;

use crate::context::{merge_project, open_journal, open_registry, resolve, run_lock_file};
use crate::error::Failure;
use crate::render;

/// `ktask-rs show`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The task to show
    pub id: u64,
    /// Print the one object `status --json` has for the task, plus its own fields, instead of
    /// text
    #[arg(long)]
    json: bool,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Prints task `args.id`, in full: its own fields, its effective provider and model, and
/// every attempt it has had.
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
    let id = TaskId(args.id);
    let detail = ktask_core::task_detail(&journal, &SystemClock, &lock, &settings, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| Failure {
            message: format!("there is no task {id}"),
            code: 2,
        })?;
    render::show(&detail, args.json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

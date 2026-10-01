//! `ktask-rs retry`: send a task that ended `failed`, `failed-unknown` or `blocked` back to
//! `pending`, so the next run picks it up again.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::TaskId;

use crate::context::{merge_project, open_queue};
use crate::error::Failure;
use crate::render;

/// `ktask-rs retry`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The ID of the task to retry
    #[arg(value_name = "ID")]
    id: u64,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Retries the task `args.id` names and prints that it was sent back to pending.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let project = merge_project(project, args.project.as_deref())?;
    let journal = open_queue(project.as_deref())?;
    ktask_core::retry_task(&journal, &SystemClock, TaskId(args.id))?;
    render::retried(TaskId(args.id), stdout)?;
    Ok(ExitCode::SUCCESS)
}

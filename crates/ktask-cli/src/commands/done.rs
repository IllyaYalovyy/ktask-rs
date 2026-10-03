//! `ktask-rs done`: mark any task `done` by hand, recording why.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::TaskId;

use crate::context::{merge_project, open_queue};
use crate::error::Failure;
use crate::render;

/// `ktask-rs done`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The ID of the task to mark done
    #[arg(value_name = "ID")]
    id: u64,
    /// Why it is done, recorded with the task
    #[arg(long, value_name = "TEXT")]
    reason: String,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Marks the task `args.id` names `done`, with `args.reason`, and prints that it was.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let project = merge_project(project, args.project.as_deref())?;
    let journal = open_queue(project.as_deref())?;
    ktask_core::done_task(&journal, &SystemClock, TaskId(args.id), &args.reason)?;
    render::done(TaskId(args.id), stdout)?;
    Ok(ExitCode::SUCCESS)
}

//! `ktask-rs remove`: remove a task from the queue — it is cancelled, and stays in the
//! journal.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::TaskId;

use crate::context::open_queue;
use crate::error::Failure;
use crate::render;

/// `ktask-rs remove`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The ID of the task to remove
    #[arg(value_name = "ID")]
    id: u64,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Cancels the task `args.id` names and prints that it was removed.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let journal = open_queue(args.project.as_deref())?;
    ktask_core::remove_task(&journal, &SystemClock, TaskId(args.id))?;
    render::removed(TaskId(args.id), stdout)?;
    Ok(ExitCode::SUCCESS)
}

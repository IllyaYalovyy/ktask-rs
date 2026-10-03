//! `ktask-rs ack`: acknowledge a pending human task, marking it done without an agent run.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::TaskId;

use crate::context::{merge_project, open_queue};
use crate::error::Failure;
use crate::render;

/// `ktask-rs ack`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The ID of the pending human task to acknowledge
    #[arg(value_name = "ID")]
    id: u64,
    /// An optional message to record with the acknowledgement
    #[arg(long, value_name = "TEXT")]
    message: Option<String>,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Acknowledges `args.id`, recording its optional message, and prints that it is done.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let project = merge_project(project, args.project.as_deref())?;
    let journal = open_queue(project.as_deref())?;
    ktask_core::acknowledge_task(
        &journal,
        &SystemClock,
        TaskId(args.id),
        args.message.as_deref(),
    )?;
    render::acknowledged(TaskId(args.id), stdout)?;
    Ok(ExitCode::SUCCESS)
}

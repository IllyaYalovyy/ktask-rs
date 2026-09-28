//! `ktask-rs list`: list the queue in order, without the tasks that were removed.

use std::io::Write;
use std::process::ExitCode;

use crate::context::open_queue;
use crate::error::Failure;
use crate::render;

/// `ktask-rs list`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Show the tasks that were removed too, with status cancelled
    #[arg(long)]
    all: bool,
    /// Print a JSON array with every field of every task instead of one line per task
    #[arg(long)]
    json: bool,
}

/// Lists the tasks of the resolved project's queue.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let journal = open_queue(args.project.as_deref())?;
    let tasks = if args.all {
        ktask_core::list_all_tasks(&journal)
    } else {
        ktask_core::list_tasks(&journal)
    }
    .map_err(|e| e.to_string())?;
    render::tasks(&tasks, args.json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

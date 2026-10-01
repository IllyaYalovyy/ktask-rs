//! `ktask-rs list`: list the queue in order, without the tasks that were removed or skipped.

use std::io::Write;
use std::process::ExitCode;

use crate::context::{merge_project, open_queue};
use crate::error::Failure;
use crate::render;

/// `ktask-rs list`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Show the tasks that were removed or skipped too, with status cancelled or skipped
    #[arg(long)]
    all: bool,
    /// Print a JSON array with every field of every task instead of one line per task
    #[arg(long)]
    json: bool,
}

/// Lists the tasks of the resolved project's queue.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let project = merge_project(project, args.project.as_deref())?;
    let journal = open_queue(project.as_deref())?;
    let tasks = if args.all {
        ktask_core::list_all_tasks(&journal)
    } else {
        ktask_core::list_tasks(&journal)
    }
    .map_err(|e| e.to_string())?;
    let width = terminal_size::terminal_size_of(std::io::stdout()).map(|(width, _)| width.0);
    render::tasks(&tasks, args.json, width, stdout)?;
    Ok(ExitCode::SUCCESS)
}

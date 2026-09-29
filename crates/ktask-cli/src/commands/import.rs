//! `ktask-rs import`: add the tasks of a JSON array, in order and all or none.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{SystemClock, read_text};

use crate::context::{merge_project, open_queue, placement};
use crate::error::Failure;
use crate::render;

/// `ktask-rs import`'s arguments.
///
/// Each task has the authored fields `list --json` prints: title, body, criteria, kind
/// and links. Only title and criteria are required. The tool-managed fields `list --json`
/// and `list --all --json` also print — `id`, `position`, `status`, `created_at` — are
/// ignored, so what one project lists imports into another unchanged; a cancelled task is
/// left out.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The JSON file to read, or - for standard input
    #[arg(value_name = "FILE")]
    file: String,
    /// Put the tasks immediately before the task with this ID
    #[arg(long, value_name = "ID", conflicts_with = "after")]
    before: Option<u64>,
    /// Put the tasks immediately after the task with this ID
    #[arg(long, value_name = "ID")]
    after: Option<u64>,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Adds every task of the JSON array `args.file` names and prints their IDs.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    // Read before anything is registered or opened, so that a missing file changes
    // nothing.
    let json = read_text(&args.file).map_err(|message| Failure { message, code: 2 })?;
    let project = merge_project(project, args.project.as_deref())?;
    let journal = open_queue(project.as_deref())?;
    let import = ktask_core::import_tasks(
        &journal,
        &SystemClock,
        &json,
        placement(args.before, args.after),
    )?;
    render::imported(&import, stdout).map_err(Failure::from)?;
    Ok(ExitCode::SUCCESS)
}

//! `ktask-rs add`: add a task, at the end of the queue unless told where.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::{TaskDraft, TaskKind};

use crate::context::{open_queue, placement};
use crate::error::{Failure, failure_from_add_problems};
use crate::render;

/// `ktask-rs add`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// One line saying what the task is
    #[arg(long, requires = "criterion")]
    title: String,
    /// What must be true for the task to be done; repeat for each criterion
    #[arg(long, value_name = "CRITERION")]
    criterion: Vec<String>,
    /// The longer description of the task
    #[arg(long)]
    body: Option<String>,
    /// Who does the task: agent or human [default: agent]
    #[arg(long, value_parser = str::parse::<TaskKind>)]
    kind: Option<TaskKind>,
    /// A related task or page: github:owner/repo#NUMBER or an http(s) URL; repeat for
    /// each link
    #[arg(long, value_name = "REF")]
    link: Vec<String>,
    /// Put the task immediately before the task with this ID
    #[arg(long, value_name = "ID", conflicts_with = "after")]
    before: Option<u64>,
    /// Put the task immediately after the task with this ID
    #[arg(long, value_name = "ID")]
    after: Option<u64>,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Adds the task `args` describes and prints its ID.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let draft = TaskDraft {
        title: args.title.clone(),
        body: args.body.clone().unwrap_or_default(),
        criteria: args.criterion.clone(),
        kind: args.kind.unwrap_or_default(),
        links: args.link.clone(),
    };
    let journal = open_queue(args.project.as_deref())?;
    let placement = placement(args.before, args.after);
    let task = ktask_core::add_task_listing_problems(&journal, &SystemClock, &draft, placement)
        .map_err(failure_from_add_problems)?;
    render::added(&task, stdout)?;
    Ok(ExitCode::SUCCESS)
}

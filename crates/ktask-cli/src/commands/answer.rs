//! `ktask-rs answer`: give the answer to the question a blocked task's attempt asked, and
//! send it back to `pending`, so the next attempt's own prompt carries both.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::TaskId;

use crate::context::{merge_project, open_queue};
use crate::error::Failure;
use crate::render;

/// `ktask-rs answer`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The ID of the blocked task to answer
    #[arg(value_name = "ID")]
    id: u64,
    /// The answer to the question its attempt asked
    #[arg(value_name = "TEXT")]
    text: String,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Records `args.text` as the answer to the question task `args.id`'s attempt asked, and
/// prints that it was sent back to pending.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let project = merge_project(project, args.project.as_deref())?;
    let journal = open_queue(project.as_deref())?;
    ktask_core::answer_task(&journal, &SystemClock, TaskId(args.id), &args.text)?;
    render::answered(TaskId(args.id), stdout)?;
    Ok(ExitCode::SUCCESS)
}

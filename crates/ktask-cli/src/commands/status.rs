//! `ktask-rs status`: what ran and how it ended.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;

use crate::context::open_queue;
use crate::error::Failure;
use crate::render;

/// `ktask-rs status`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Print a JSON array instead of one line per task
    #[arg(long)]
    json: bool,
}

/// Prints every task that was attempted, with its most recent attempt.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let journal = open_queue(args.project.as_deref())?;
    let entries = ktask_core::status(&journal, &SystemClock).map_err(|e| e.to_string())?;
    render::status(&entries, args.json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

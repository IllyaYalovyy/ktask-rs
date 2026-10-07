//! `ktask-rs import`: add the tasks of a JSON array, in order and all or none.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{SystemClock, read_text};
use ktask_core::TaskFormat;

use crate::context::{merge_project, open_journal, open_registry, placement, resolve};
use crate::error::Failure;
use crate::render;

/// `ktask-rs import`'s arguments.
///
/// Each task has the authored fields `list --json` prints: title, body, criteria, kind, links,
/// provider and model. Only title and criteria are required. The tool-managed fields `list --json`
/// and `list --all --json` also print — `id`, `position`, `status`, `created_at` — are
/// ignored, so what one project lists imports into another unchanged; a cancelled task is
/// left out.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The .json or .toml file to read, or - for standard input (JSON)
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
    let format = TaskFormat::of_path(&args.file)?;
    let text = read_text(&args.file).map_err(|message| Failure { message, code: 2 })?;
    let project = merge_project(project, args.project.as_deref())?;
    let registry = open_registry()?;
    let (project, settings) = resolve(&registry, project.as_deref())?;
    let known = ktask_core::show_providers(&settings, &ktask_adapters::builtin_providers())
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|provider| provider.name)
        .collect::<Vec<_>>();
    let journal = open_journal(&project)?;
    let import = ktask_core::import_tasks_with_providers(
        &journal,
        &SystemClock,
        &text,
        format,
        placement(args.before, args.after),
        &known,
    )?;
    render::imported(&import, stdout).map_err(Failure::from)?;
    Ok(ExitCode::SUCCESS)
}

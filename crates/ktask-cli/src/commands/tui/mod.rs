//! `ktask-rs tui`: open the terminal interface on the project's queue.

use std::fmt;
use std::io::{self, IsTerminal};
use std::path::Path;

use ktask_adapters::{GitCli, SqliteJournal, SqliteRegistry, SystemClock};
use ktask_core::{Import, ImportError, Placement, Project, ResolveError};

use crate::context::{current_dir, current_exe, merge_project, open_registry, resolved};
use crate::error::Failure;

mod application;
mod process;

/// Why importing through the file the import form was submitted with added nothing: it could
/// not be read, or the same reason `ktask-rs import` itself would refuse for, keeping that
/// error's own meaning until it is shown.
#[derive(Debug)]
pub(super) enum ImportProblem {
    Read(String),
    Import(ImportError),
}

impl fmt::Display for ImportProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(message) => f.write_str(message),
            Self::Import(error) => error.fmt(f),
        }
    }
}

/// `ktask-rs tui`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// What the terminal interface opens on: the project's queue, already resolved, or a name still
/// needed to register the current directory under, because its own folder name is already
/// registered for another path.
enum Start {
    Ready(Project),
    NameTaken(String),
}

/// Opens the terminal interface on the queue of the project selected, or the current one — or,
/// when the current directory's own folder name is already taken by another registered path,
/// asks for a name to register it under instead of refusing outright, since there is a terminal
/// right here to ask on; the outcome either way is the one `ktask-rs project register --name`
/// gives for the same name.
pub(crate) fn run(args: &Args, project: Option<&str>) -> Result<(), Failure> {
    ensure_terminal()?;
    let registry = open_registry()?;
    let selected = merge_project(project, args.project.as_deref())?;
    let cwd = current_dir()?;
    let start = resolve_or_ask_to_register(&registry, &cwd, selected.as_deref())?;
    let binary_path = current_exe()?;
    Ok(application::drive(registry, cwd, start, binary_path)?)
}

/// Resolves the project the terminal interface opens on, exactly as every other command does,
/// except that a folder name already taken by another path is not refused outright: it is
/// handed to the screen instead, to ask for a name interactively.
fn resolve_or_ask_to_register(
    registry: &SqliteRegistry,
    cwd: &Path,
    selected: Option<&str>,
) -> Result<Start, Failure> {
    match ktask_core::resolve_project(registry, &GitCli, &SystemClock, cwd, selected) {
        Ok(resolution) => {
            let (project, _settings) = resolved(resolution)?;
            Ok(Start::Ready(project))
        }
        Err(error @ ResolveError::NameTaken { .. }) => Ok(Start::NameTaken(error.to_string())),
        Err(other) => Err(other.into()),
    }
}

/// Reads the file `path` names as text, refusing with the same wording `ktask-rs import`
/// gives for the same problem.
fn read_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))
}

/// Imports the tasks of the file `path` names into `journal`, at the end of the queue, giving
/// the same typed result `ktask_core::import_tasks` itself gives, for the screen to word as it
/// shows it.
fn import_into(journal: &SqliteJournal, path: &str) -> Result<Import, ImportProblem> {
    let json = read_file(path).map_err(ImportProblem::Read)?;
    ktask_core::import_tasks(journal, &SystemClock, &json, Placement::End)
        .map_err(ImportProblem::Import)
}

/// Refuses to open the terminal interface when there is no terminal to draw it on.
fn ensure_terminal() -> Result<(), Failure> {
    if io::stdout().is_terminal() {
        return Ok(());
    }
    Err(Failure {
        message: "the terminal interface needs a terminal; \
                  `ktask-rs list` shows the queue without one"
            .to_owned(),
        code: 2,
    })
}

//! What every command needs to find its project and state: the registry, a project's
//! journal, and the paths they live at.

use std::io;
use std::path::PathBuf;

use ktask_adapters::{
    GitCli, SqliteJournal, SqliteRegistry, SystemClock, journal_path, registry_path, run_lock_path,
};
use ktask_core::{Placement, Project, TaskId};

use crate::error::Failure;
use crate::render;

/// Where `--before` and `--after` put new tasks.
pub(crate) fn placement(before: Option<u64>, after: Option<u64>) -> Placement {
    match (before, after) {
        (Some(id), _) => Placement::Before(TaskId(id)),
        (None, Some(id)) => Placement::After(TaskId(id)),
        (None, None) => Placement::End,
    }
}

/// The journal of the project a command works on.
pub(crate) fn open_queue(selected: Option<&str>) -> Result<SqliteJournal, Failure> {
    let registry = open_registry()?;
    let project = resolve(&registry, selected)?;
    Ok(open_journal(&project)?)
}

/// The project a command works on, telling on standard error when that registered it.
pub(crate) fn resolve(
    registry: &SqliteRegistry,
    selected: Option<&str>,
) -> Result<Project, Failure> {
    let cwd = current_dir()?;
    let resolution = ktask_core::resolve_project(registry, &GitCli, &SystemClock, &cwd, selected)?;
    if resolution.registered {
        render::registered(&resolution.project, &mut io::stderr())?;
    }
    Ok(resolution.project)
}

pub(crate) fn open_registry() -> Result<SqliteRegistry, String> {
    let path = registry_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")).ok_or(
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path",
    )?;
    SqliteRegistry::open(&path).map_err(|e| e.to_string())
}

/// Where the journal of `project` lives.
pub(crate) fn journal_file(project: &Project) -> Result<PathBuf, String> {
    journal_path(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &project.name,
    )
    .ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })
}

pub(crate) fn open_journal(project: &Project) -> Result<SqliteJournal, String> {
    SqliteJournal::open(&journal_file(project)?).map_err(|e| e.to_string())
}

/// Where the run lock of `project` lives.
pub(crate) fn run_lock_file(project: &Project) -> Result<PathBuf, String> {
    run_lock_path(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &project.name,
    )
    .ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })
}

pub(crate) fn current_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("cannot find the current directory: {e}"))
}

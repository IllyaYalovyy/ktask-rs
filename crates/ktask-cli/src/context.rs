//! What every command needs to find its project and state: the registry, a project's
//! journal, and the paths they live at.

use std::io;
use std::path::PathBuf;

use ktask_adapters::{
    GitCli, SqliteJournal, SqliteRegistry, SystemClock, TomlSettingsStore, journal_path,
    outputs_dir_path, registry_path, run_lock_path, sessions_dir_path, settings_path,
    state_root_path,
};
use ktask_core::{Placement, Project, Settings, SettingsStore, TaskId};

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
    let (project, _settings) = resolve(&registry, selected)?;
    Ok(open_journal(&project)?)
}

/// The project a command works on, telling on standard error when that registered it, and
/// its settings — read here so that a settings file that cannot be read stops the command at
/// once, whether or not it goes on to use the settings themselves.
pub(crate) fn resolve(
    registry: &SqliteRegistry,
    selected: Option<&str>,
) -> Result<(Project, Settings), Failure> {
    let cwd = current_dir()?;
    let resolution = ktask_core::resolve_project(registry, &GitCli, &SystemClock, &cwd, selected)?;
    resolved(resolution)
}

/// What [`resolve`] does once a [`ktask_core::Resolution`] is in hand — telling on standard
/// error when it registered the project, and reading its settings so that a settings file that
/// cannot be read stops the command at once. Also used by `ktask-rs tui`, which resolves the
/// project itself so it can ask for a name interactively when the folder name is already taken,
/// instead of refusing outright.
pub(crate) fn resolved(resolution: ktask_core::Resolution) -> Result<(Project, Settings), Failure> {
    if resolution.registered {
        render::registered(&resolution.project, &mut io::stderr())?;
    }
    let settings = open_settings_store(&resolution.project)?
        .load()
        .map_err(|e| e.to_string())?;
    Ok((resolution.project, settings))
}

pub(crate) fn open_registry() -> Result<SqliteRegistry, String> {
    let path = registry_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")).ok_or(
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path",
    )?;
    SqliteRegistry::open(&path).map_err(|e| e.to_string())
}

/// The directory every registered project's own state lives under, for watching every one's
/// journal at once.
pub(crate) fn state_root() -> Result<PathBuf, String> {
    state_root_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")).ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })
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

/// Where the session transcripts of `project` live.
pub(crate) fn sessions_dir_file(project: &Project) -> Result<PathBuf, String> {
    sessions_dir_path(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &project.name,
    )
    .ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })
}

/// Where the live and retained output of `project`'s attempts lives.
pub(crate) fn outputs_dir_file(project: &Project) -> Result<PathBuf, String> {
    outputs_dir_path(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &project.name,
    )
    .ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })
}

/// Where the settings of `project` live.
pub(crate) fn settings_file(project: &Project) -> Result<PathBuf, String> {
    settings_path(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &project.name,
    )
    .ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })
}

/// The settings store of `project`.
pub(crate) fn open_settings_store(project: &Project) -> Result<TomlSettingsStore, String> {
    Ok(TomlSettingsStore::new(settings_file(project)?))
}

/// Combines the project named before the subcommand with the one named after it — either
/// position means the same thing, so a command works whichever way round `--project` is
/// typed. Refuses when both were given and disagree.
pub(crate) fn merge_project(
    before: Option<&str>,
    after: Option<&str>,
) -> Result<Option<String>, Failure> {
    match (before, after) {
        (Some(before), Some(after)) if before != after => Err(Failure {
            message: format!("the project was named twice, as {before:?} and {after:?}"),
            code: 2,
        }),
        (Some(value), _) | (None, Some(value)) => Ok(Some(value.to_owned())),
        (None, None) => Ok(None),
    }
}

/// Refuses a project named before a command that does not work on one — the same refusal
/// clap gives when `--project` is named after such a command, where it is not a declared
/// option.
pub(crate) fn reject_project(project: Option<&str>) -> Result<(), Failure> {
    match project {
        None => Ok(()),
        Some(_) => Err(Failure {
            message: "unexpected argument '--project' found".to_owned(),
            code: 2,
        }),
    }
}

pub(crate) fn current_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("cannot find the current directory: {e}"))
}

/// The path of the running `ktask-rs` binary, so a prompt can tell an agent exactly which one
/// to call back into, whatever is or is not on its `PATH`.
pub(crate) fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot find the running binary: {e}"))
}

//! Turns use-case results into the text and JSON the commands print.

use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use ktask_core::{AttemptToken, Outcome, Output, Project, Task, TaskId};
use serde::Serialize;

mod run;
mod settings;
mod status;
mod tasks;

pub(crate) use run::run;
pub(crate) use settings::{setting_set, settings};
pub(crate) use status::status;
pub(crate) use tasks::tasks;

/// One project as `project list --json` shows it.
#[derive(Debug, Serialize)]
struct ProjectJson<'a> {
    name: &'a str,
    path: &'a Path,
    registered_at: String,
}

/// One project as `project show --json` shows it.
#[derive(Debug, Serialize)]
struct ShownJson<'a> {
    name: &'a str,
    path: &'a Path,
}

/// Writes the line telling that `project` has just been registered.
pub(crate) fn registered(project: &Project, out: &mut impl Write) -> Result<(), String> {
    writeln!(
        out,
        "registered project {} → {}",
        project.name,
        project.path.display()
    )
    .map_err(|e| e.to_string())
}

/// Writes the line telling that `project` has just been forgotten, and where its journal was
/// left, since forgetting a project does not remove it.
pub(crate) fn forgotten(
    project: &Project,
    journal: &Path,
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(
        out,
        "forgot project {}; its journal stays at {}",
        project.name,
        journal.display()
    )
    .map_err(|e| e.to_string())
}

/// Writes the line telling that `name` was not forgotten because the confirmation was
/// declined.
pub(crate) fn forget_declined(name: &str, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "project {name:?} was not forgotten").map_err(|e| e.to_string())
}

/// Writes `project`: a `name<TAB>path` line, or a JSON object with `json`.
pub(crate) fn project(project: &Project, json: bool, out: &mut impl Write) -> Result<(), String> {
    if json {
        let shown = ShownJson {
            name: &project.name,
            path: &project.path,
        };
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        writeln!(out, "{}\t{}", project.name, project.path.display()).map_err(|e| e.to_string())
    }
}

/// Writes `projects`: one `name<TAB>path` line each, or a JSON array with `json`.
pub(crate) fn projects(
    projects: &[Project],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        let shown = projects
            .iter()
            .map(|project| {
                let registered_at = Timestamp::try_from(project.registered_at)
                    .map_err(|e| format!("project {}: bad registration time: {e}", project.name))?;
                Ok(ProjectJson {
                    name: &project.name,
                    path: &project.path,
                    registered_at: registered_at.to_string(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        projects.iter().try_for_each(|project| {
            writeln!(out, "{}\t{}", project.name, project.path.display()).map_err(|e| e.to_string())
        })
    }
}

/// Writes what a provider produced: its standard output to `stdout`, its standard error to
/// `stderr`.
pub(crate) fn provider_output(
    output: &Output,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), String> {
    stdout
        .write_all(&output.stdout)
        .map_err(|e| e.to_string())?;
    stderr.write_all(&output.stderr).map_err(|e| e.to_string())
}

/// Writes the ID of the task that has just been added.
pub(crate) fn added(task: &Task, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "{}", task.id).map_err(|e| e.to_string())
}

/// Writes `import`: one line saying how many tasks were added and their IDs, when any were,
/// then, when any cancelled task was left out, one line saying how many.
pub(crate) fn imported(import: &ktask_core::Import, out: &mut impl Write) -> Result<(), String> {
    if let Some(message) = import.added_message() {
        writeln!(out, "{message}").map_err(|e| e.to_string())?;
    }
    if let Some(message) = import.skipped_message() {
        writeln!(out, "{message}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Writes the line telling that the task numbered `id` has just been removed.
pub(crate) fn removed(id: TaskId, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "removed task {id}").map_err(|e| e.to_string())
}

/// Writes the line telling that the task numbered `id` has just been sent back to pending.
pub(crate) fn retried(id: TaskId, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "task {id} is pending again").map_err(|e| e.to_string())
}

/// Writes the line telling that the task numbered `id` has just been given its answer and
/// sent back to pending.
pub(crate) fn answered(id: TaskId, out: &mut impl Write) -> Result<(), String> {
    writeln!(
        out,
        "recorded the answer for task {id}; it is pending again"
    )
    .map_err(|e| e.to_string())
}

/// Writes the line telling that the task numbered `id` has just been marked done by hand.
pub(crate) fn done(id: TaskId, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "task {id} is done").map_err(|e| e.to_string())
}

/// Writes the line telling that `outcome` has just been recorded for the attempt `token`
/// names.
pub(crate) fn reported(
    token: &AttemptToken,
    outcome: Outcome,
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(
        out,
        "recorded {outcome} for task {} attempt {}",
        token.task, token.number
    )
    .map_err(|e| e.to_string())
}

//! Turns use-case results into the text and JSON the commands print.

use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use ktask_core::{AttemptToken, Outcome, Output, Project, RunEnd, RunReport, Task, TaskId};
use serde::Serialize;

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

/// Writes the line telling that the task numbered `id` has just been removed.
pub(crate) fn removed(id: TaskId, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "removed task {id}").map_err(|e| e.to_string())
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

/// Writes what a run did: one line per task attempted, then a line saying why it ended when
/// there was nothing left to attempt or a task of kind `human` stopped it. Returns whether
/// the run stopped on a failing ending — `failed`, `blocked` or `failed-unknown` — which the
/// caller reports with exit code 1.
pub(crate) fn run(report: &RunReport, out: &mut impl Write) -> Result<bool, String> {
    for attempt in &report.attempted {
        match &attempt.reason {
            Some(reason) => writeln!(out, "task {}: {}: {reason}", attempt.id, attempt.status),
            None => writeln!(out, "task {}: {}", attempt.id, attempt.status),
        }
        .map_err(|e| e.to_string())?;
    }
    match &report.end {
        RunEnd::EmptyQueue => writeln!(out, "the queue is empty").map_err(|e| e.to_string())?,
        RunEnd::NothingPending => {
            writeln!(out, "nothing is pending").map_err(|e| e.to_string())?;
        }
        RunEnd::HumanTask(id) => {
            writeln!(out, "task {id} is a human task; run stopped").map_err(|e| e.to_string())?;
        }
        RunEnd::Completed | RunEnd::Stopped { .. } => {}
    }
    Ok(matches!(report.end, RunEnd::Stopped { .. }))
}

/// One task as `list --json` shows it.
#[derive(Debug, Serialize)]
struct TaskJson<'a> {
    id: u64,
    position: usize,
    title: &'a str,
    body: &'a str,
    criteria: &'a [String],
    kind: &'static str,
    links: &'a [String],
    status: &'static str,
    created_at: String,
}

/// Writes `tasks`: one `position<TAB>#ID<TAB>status<TAB>kind<TAB>title` line each, or a JSON
/// array with `json`.
pub(crate) fn tasks(tasks: &[Task], json: bool, out: &mut impl Write) -> Result<(), String> {
    if json {
        let shown = tasks
            .iter()
            .map(|task| {
                let created_at = Timestamp::try_from(task.created_at)
                    .map_err(|e| format!("task {}: bad creation time: {e}", task.id))?;
                Ok(TaskJson {
                    id: task.id.0,
                    position: task.position,
                    title: &task.title,
                    body: &task.body,
                    criteria: &task.criteria,
                    kind: task.kind.as_str(),
                    links: &task.links,
                    status: task.status.as_str(),
                    created_at: created_at.to_string(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        tasks.iter().try_for_each(|task| {
            writeln!(
                out,
                "{}\t#{}\t{}\t{}\t{}",
                task.position, task.id, task.status, task.kind, task.title
            )
            .map_err(|e| e.to_string())
        })
    }
}

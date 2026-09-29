//! Turns use-case results into the text and JSON the commands print.

use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use ktask_core::{
    AttemptToken, Outcome, Output, Project, RunEnd, RunReport, SettingView, StatusEntry, Task,
    TaskId,
};
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
/// there was nothing left to attempt, a task of kind `human` stopped it, or an earlier task
/// left `failed`, `blocked` or `failed-unknown` refused it. Returns whether the run stopped
/// on a failing ending — `failed`, `blocked` or `failed-unknown`, whether from an attempt
/// this run made or one an earlier run already left behind — which the caller reports with
/// exit code 1.
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
        RunEnd::Blocked { id, status, reason } => {
            match reason {
                Some(reason) => writeln!(out, "task {id}: {status}: {reason}; run did not start"),
                None => writeln!(out, "task {id}: {status}; run did not start"),
            }
            .map_err(|e| e.to_string())?;
        }
        RunEnd::Completed | RunEnd::Stopped { .. } => {}
    }
    Ok(matches!(
        report.end,
        RunEnd::Stopped { .. } | RunEnd::Blocked { .. }
    ))
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

/// One task's attempt as `status --json` shows it.
#[derive(Debug, Serialize)]
struct AttemptJson<'a> {
    number: u32,
    step: &'a str,
    provider: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    reason: Option<&'a str>,
}

/// One task as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StatusJson<'a> {
    id: u64,
    title: &'a str,
    status: &'static str,
    attempt: AttemptJson<'a>,
}

/// Writes `entries`: for every task that was attempted, one `#ID<TAB>status<TAB>title` line
/// followed by an indented line for its attempt — step, provider, time spent, outcome, and
/// the reason when it did not succeed — or a JSON array with `json`. Nothing is written when
/// `entries` is empty.
pub(crate) fn status(
    entries: &[StatusEntry],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        status_json(entries, out)
    } else {
        status_text(entries, out)
    }
}

/// Writes `entries` as a JSON array.
fn status_json(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    let shown: Vec<_> = entries
        .iter()
        .map(|entry| StatusJson {
            id: entry.task.0,
            title: &entry.title,
            status: ktask_core::displayed_status(entry.status, Some(entry.attempt.outcome)),
            attempt: AttemptJson {
                number: entry.attempt.number,
                step: &entry.attempt.step,
                provider: entry.attempt.provider.as_deref(),
                time_spent_seconds: entry.attempt.time_spent.as_secs(),
                outcome: entry.attempt.outcome.as_str(),
                reason: entry.attempt.reason.as_deref(),
            },
        })
        .collect();
    serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

/// One setting as `settings --json` shows it.
#[derive(Debug, Serialize)]
struct SettingJson<'a> {
    name: &'a str,
    value: u64,
    default: bool,
}

/// Writes `views`: one `name<TAB>value<TAB>default|custom` line each, or a JSON array with
/// `json`.
pub(crate) fn settings(
    views: &[SettingView],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        let shown: Vec<_> = views
            .iter()
            .map(|view| SettingJson {
                name: view.name,
                value: view.value,
                default: view.is_default,
            })
            .collect();
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        views.iter().try_for_each(|view| {
            let kind = if view.is_default { "default" } else { "custom" };
            writeln!(out, "{}\t{}\t{kind}", view.name, view.value).map_err(|e| e.to_string())
        })
    }
}

/// Writes `view`, the setting `settings set` has just changed: a `name<TAB>value` line, or
/// a JSON object with `json`.
pub(crate) fn setting_set(
    view: &SettingView,
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        let shown = SettingJson {
            name: view.name,
            value: view.value,
            default: view.is_default,
        };
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        writeln!(out, "{}\t{}", view.name, view.value).map_err(|e| e.to_string())
    }
}

/// Writes `entries`: for every task, one `#ID<TAB>status<TAB>title` line followed by an
/// indented line for its attempt — step, provider, time spent, outcome, and the reason
/// when it did not succeed.
fn status_text(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    entries
        .iter()
        .try_for_each(|entry| {
            let status = ktask_core::displayed_status(entry.status, Some(entry.attempt.outcome));
            writeln!(out, "#{}\t{}\t{}", entry.task, status, entry.title)?;
            let provider = entry.attempt.provider.as_deref().unwrap_or("-");
            match &entry.attempt.reason {
                Some(reason) => writeln!(
                    out,
                    "\t{}\t{}\t{}s\t{}\t{reason}",
                    entry.attempt.step,
                    provider,
                    entry.attempt.time_spent.as_secs(),
                    entry.attempt.outcome
                ),
                None => writeln!(
                    out,
                    "\t{}\t{}\t{}s\t{}",
                    entry.attempt.step,
                    provider,
                    entry.attempt.time_spent.as_secs(),
                    entry.attempt.outcome
                ),
            }
        })
        .map_err(|e: std::io::Error| e.to_string())
}

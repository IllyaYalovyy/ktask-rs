//! Rendering what a run did, and why it ended or refused to start.

use std::io::Write;

use ktask_core::{RunEnd, RunReport, SyncProblem, TaskId};

use crate::run_report_json::RunReportJson;

/// Whether `report` ended on a failing ending — `failed`, `blocked` or `failed-unknown`,
/// whether from an attempt this run made or one an earlier run already left behind — which
/// the caller reports with exit code 1, whichever way `report` is shown.
pub(crate) fn stopped(report: &RunReport) -> bool {
    matches!(
        report.end,
        RunEnd::Stopped { .. }
            | RunEnd::Blocked { .. }
            | RunEnd::HealthCheckFailed { .. }
            | RunEnd::SyncFailed { .. }
            | RunEnd::InstructionsUnreadable { .. }
    )
}

/// Writes `report`: a JSON object with `json`, for `ktask-rs run --json` and for the terminal
/// interface, which reads it back into the same typed report to word as it shows it; text
/// otherwise — one line per task attempted, then a line saying why it ended when there was
/// nothing left to attempt, a task of kind `human` stopped it, or an earlier task left
/// `failed`, `blocked` or `failed-unknown` refused it. Returns whether `report` ended on a
/// failing ending, [`stopped`]'s own answer, which the caller reports with exit code 1.
pub(crate) fn run(report: &RunReport, json: bool, out: &mut impl Write) -> Result<bool, String> {
    if json {
        serde_json::to_writer(&mut *out, &RunReportJson::from(report))
            .map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())?;
        return Ok(stopped(report));
    }
    for attempt in &report.attempted {
        match &attempt.reason {
            Some(reason) => writeln!(out, "task {}: {}: {reason}", attempt.id, attempt.status),
            None => writeln!(out, "task {}: {}", attempt.id, attempt.status),
        }
        .map_err(|e| e.to_string())?;
    }
    run_end(&report.end, out)?;
    Ok(stopped(report))
}

/// Writes the line, or lines, saying why a run ended at `end` — nothing for `Completed` or
/// `Stopped`, whose own attempt line, written by [`run`] before this is reached, already said
/// so.
fn run_end(end: &RunEnd, out: &mut impl Write) -> Result<(), String> {
    match end {
        RunEnd::EmptyQueue => writeln!(out, "the queue is empty").map_err(|e| e.to_string()),
        RunEnd::NothingPending => writeln!(out, "nothing is pending").map_err(|e| e.to_string()),
        RunEnd::HumanTask(id) => {
            writeln!(out, "task {id} is a human task; run stopped").map_err(|e| e.to_string())
        }
        RunEnd::Blocked { id, status, reason } => match reason {
            Some(reason) => writeln!(out, "task {id}: {status}: {reason}; run did not start"),
            None => writeln!(out, "task {id}: {status}; run did not start"),
        }
        .map_err(|e| e.to_string()),
        RunEnd::HealthCheckFailed {
            id,
            command,
            reason,
            output_tail,
        } => health_check_failed(*id, command, reason, output_tail, out),
        RunEnd::SyncFailed {
            id,
            tracked_branch,
            problem,
        } => sync_failed(*id, tracked_branch, problem, out),
        RunEnd::InstructionsUnreadable { id, path, reason } => {
            instructions_unreadable(*id, path, reason, out)
        }
        RunEnd::Completed | RunEnd::Stopped { .. } => Ok(()),
    }
}

/// Writes why the health check ahead of task `id` failed: the command, why, the end of what it
/// printed, and that the task was not started because of it.
fn health_check_failed(
    id: TaskId,
    command: &str,
    reason: &str,
    output_tail: &str,
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(out, "task {id}: health check failed: {command}: {reason}")
        .map_err(|e| e.to_string())?;
    if !output_tail.is_empty() {
        writeln!(out, "{output_tail}").map_err(|e| e.to_string())?;
    }
    writeln!(
        out,
        "task {id} was not started; fix the health check, then run again"
    )
    .map_err(|e| e.to_string())
}

/// Writes why the instruction file `path` ahead of task `id` could not be read, and that the task
/// was not started because of it.
fn instructions_unreadable(
    id: TaskId,
    path: &str,
    reason: &str,
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(
        out,
        "task {id}: instructions: {path} could not be read ({reason})"
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "task {id} was not started; add the file or change instructions-dir, then run again"
    )
    .map_err(|e| e.to_string())
}

/// Writes why the sync refused for [`SyncProblem::UncommittedChanges`].
fn sync_failed_uncommitted(id: TaskId, status: &str, out: &mut impl Write) -> Result<(), String> {
    writeln!(
        out,
        "task {id}: sync: the project's directory has uncommitted changes:"
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "{status}").map_err(|e| e.to_string())?;
    writeln!(
        out,
        "task {id} was not started; commit or stash your changes, then run again"
    )
    .map_err(|e| e.to_string())
}

/// Writes why the sync refused for [`SyncProblem::RemoteUnreachable`].
fn sync_failed_remote_unreachable(
    id: TaskId,
    tracked_branch: &str,
    reason: &str,
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(
        out,
        "task {id}: sync: {tracked_branch}'s remote could not be reached: {reason}"
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "task {id} was not started; make the remote reachable, then run again"
    )
    .map_err(|e| e.to_string())
}

/// Writes why the sync refused for [`SyncProblem::Conflict`].
fn sync_failed_conflict(
    id: TaskId,
    tracked_branch: &str,
    files: &[String],
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(
        out,
        "task {id}: sync: rebasing onto {tracked_branch} conflicted in:"
    )
    .map_err(|e| e.to_string())?;
    for file in files {
        writeln!(out, "  {file}").map_err(|e| e.to_string())?;
    }
    writeln!(
        out,
        "the rebase was undone; the project's directory is exactly as it was"
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "task {id} was not started; resolve the conflict yourself \
         (pull --rebase, fix, push), then run again"
    )
    .map_err(|e| e.to_string())
}

/// Writes why the sync refused for [`SyncProblem::GitFailed`].
fn sync_failed_git(id: TaskId, reason: &str, out: &mut impl Write) -> Result<(), String> {
    writeln!(out, "task {id}: sync: {reason}").map_err(|e| e.to_string())?;
    writeln!(
        out,
        "task {id} was not started; fix the problem, then run again"
    )
    .map_err(|e| e.to_string())
}

/// Writes why the sync ahead of task `id`'s health check refused to pull `tracked_branch`, and
/// what the operator is expected to do about it.
fn sync_failed(
    id: TaskId,
    tracked_branch: &str,
    problem: &SyncProblem,
    out: &mut impl Write,
) -> Result<(), String> {
    match problem {
        SyncProblem::UncommittedChanges(status) => sync_failed_uncommitted(id, status, out),
        SyncProblem::RemoteUnreachable(reason) => {
            sync_failed_remote_unreachable(id, tracked_branch, reason, out)
        }
        SyncProblem::Conflict(files) => sync_failed_conflict(id, tracked_branch, files, out),
        SyncProblem::GitFailed(reason) => sync_failed_git(id, reason, out),
    }
}

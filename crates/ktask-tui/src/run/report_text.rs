//! Turns a run's or an import's own typed result into the words `ktask-rs run` or
//! `ktask-rs import` itself would print: built here, from the typed values
//! [`crate::application::Application::start_run`] and
//! [`crate::application::Application::import`] give, not received already rendered into text
//! from anywhere upstream of this crate.

use ktask_core::{Attempted, Import, RunEnd, RunReport, SyncProblem, TaskId};

/// The words for `import`'s own result: how many tasks were added and their IDs, then, when
/// any cancelled task was left out, one line saying how many — the same words `ktask-rs
/// import` itself prints.
pub(super) fn import_text(import: &Import) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.extend(import.added_message());
    lines.extend(import.skipped_message());
    lines.join("\n")
}

/// The words for a run's own report: one line per task attempted, then, when it ended without
/// attempting anything else, why — the same words `ktask-rs run` itself prints.
pub(super) fn report_text(report: &RunReport) -> String {
    let mut lines: Vec<String> = report.attempted.iter().map(attempted_line).collect();
    lines.extend(end_lines(&report.end));
    lines.join("\n")
}

/// The line for one task a run attempted: its status, and why when it was not `done`.
fn attempted_line(attempt: &Attempted) -> String {
    match &attempt.reason {
        Some(reason) => format!("task {}: {}: {reason}", attempt.id, attempt.status),
        None => format!("task {}: {}", attempt.id, attempt.status),
    }
}

/// The line, or lines, saying why a run ended at `end` — none for `Completed` or `Stopped`,
/// whose own attempt line already said so.
fn end_lines(end: &RunEnd) -> Vec<String> {
    match end {
        RunEnd::EmptyQueue => vec!["the queue is empty".to_owned()],
        RunEnd::NothingPending => vec!["nothing is pending".to_owned()],
        RunEnd::HumanTask(id) => vec![format!("task {id} is a human task; run stopped")],
        RunEnd::Blocked { id, status, reason } => vec![match reason {
            Some(reason) => format!("task {id}: {status}: {reason}; run did not start"),
            None => format!("task {id}: {status}; run did not start"),
        }],
        RunEnd::HealthCheckFailed {
            id,
            command,
            reason,
            output_tail,
        } => health_check_failed_lines(*id, command, reason, output_tail),
        RunEnd::SyncFailed {
            id,
            tracked_branch,
            problem,
        } => sync_failed_lines(*id, tracked_branch, problem),
        RunEnd::InstructionsUnreadable { id, path, reason } => vec![
            format!("task {id}: instructions: {path} could not be read ({reason})"),
            format!(
                "task {id} was not started; add the file or change instructions-dir, then run again"
            ),
        ],
        RunEnd::Completed | RunEnd::Stopped { .. } => Vec::new(),
    }
}

/// The lines saying why the health check ahead of task `id` failed: the command, why, the end
/// of what it printed, and that the task was not started because of it.
fn health_check_failed_lines(
    id: TaskId,
    command: &str,
    reason: &str,
    output_tail: &str,
) -> Vec<String> {
    let mut lines = vec![format!(
        "task {id}: health check failed: {command}: {reason}"
    )];
    if !output_tail.is_empty() {
        lines.push(output_tail.to_owned());
    }
    lines.push(format!(
        "task {id} was not started; fix the health check, then run again"
    ));
    lines
}

/// The lines saying why the sync ahead of task `id`'s health check refused to pull
/// `tracked_branch`, and what the operator is expected to do about it.
fn sync_failed_lines(id: TaskId, tracked_branch: &str, problem: &SyncProblem) -> Vec<String> {
    match problem {
        SyncProblem::UncommittedChanges(status) => vec![
            format!("task {id}: sync: the project's directory has uncommitted changes:"),
            status.clone(),
            format!("task {id} was not started; commit or stash your changes, then run again"),
        ],
        SyncProblem::RemoteUnreachable(reason) => vec![
            format!("task {id}: sync: {tracked_branch}'s remote could not be reached: {reason}"),
            format!("task {id} was not started; make the remote reachable, then run again"),
        ],
        SyncProblem::Conflict(files) => {
            let mut lines = vec![format!(
                "task {id}: sync: rebasing onto {tracked_branch} conflicted in:"
            )];
            lines.extend(files.iter().map(|file| format!("  {file}")));
            lines.push(
                "the rebase was undone; the project's directory is exactly as it was".to_owned(),
            );
            lines.push(format!(
                "task {id} was not started; resolve the conflict yourself \
                 (pull --rebase, fix, push), then run again"
            ));
            lines
        }
        SyncProblem::GitFailed(reason) => vec![
            format!("task {id}: sync: {reason}"),
            format!("task {id} was not started; fix the problem, then run again"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use ktask_core::{Task, TaskKind, TaskStatus};

    use super::*;

    fn task(id: u64) -> Task {
        Task {
            id: TaskId(id),
            position: 0,
            title: "t".to_owned(),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            provider: None,
            model: None,
            status: TaskStatus::Done,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn import_text_names_the_count_and_ids_then_the_skipped_message() {
        let import = Import {
            tasks: vec![task(1), task(2)],
            skipped_cancelled: 1,
        };
        assert_eq!(
            import_text(&import),
            "2 tasks added: 1, 2\n1 cancelled task was skipped"
        );
    }

    #[test]
    fn import_text_with_nothing_skipped_has_no_extra_line() {
        let import = Import {
            tasks: vec![task(7)],
            skipped_cancelled: 0,
        };
        assert_eq!(import_text(&import), "1 task added: 7");
    }

    #[test]
    fn report_text_shows_one_line_per_attempt_with_its_reason_when_it_has_one() {
        let report = RunReport {
            attempted: vec![
                Attempted {
                    id: TaskId(1),
                    status: TaskStatus::Done,
                    reason: None,
                },
                Attempted {
                    id: TaskId(2),
                    status: TaskStatus::Failed,
                    reason: Some("it broke".to_owned()),
                },
            ],
            end: RunEnd::Stopped {
                id: TaskId(2),
                status: TaskStatus::Failed,
            },
        };
        assert_eq!(
            report_text(&report),
            "task 1: done\ntask 2: failed: it broke"
        );
    }

    #[test]
    fn report_text_for_nothing_pending_is_one_line_and_nothing_attempted() {
        let report = RunReport {
            attempted: vec![],
            end: RunEnd::NothingPending,
        };
        assert_eq!(report_text(&report), "nothing is pending");
    }

    #[test]
    fn report_text_for_blocked_ends_with_run_did_not_start() {
        let report = RunReport {
            attempted: vec![],
            end: RunEnd::Blocked {
                id: TaskId(1),
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
            },
        };
        assert_eq!(
            report_text(&report),
            "task 1: failed: it broke; run did not start"
        );
    }

    #[test]
    fn report_text_for_a_sync_conflict_lists_every_file() {
        let report = RunReport {
            attempted: vec![],
            end: RunEnd::SyncFailed {
                id: TaskId(3),
                tracked_branch: "origin/main".to_owned(),
                problem: SyncProblem::Conflict(vec!["a.rs".to_owned(), "b.rs".to_owned()]),
            },
        };
        assert_eq!(
            report_text(&report),
            "task 3: sync: rebasing onto origin/main conflicted in:\n  a.rs\n  b.rs\n\
             the rebase was undone; the project's directory is exactly as it was\n\
             task 3 was not started; resolve the conflict yourself \
             (pull --rebase, fix, push), then run again"
        );
    }
}

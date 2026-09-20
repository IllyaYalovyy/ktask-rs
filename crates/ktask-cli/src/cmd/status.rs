//! Status command: show the status of all tasks.

use crate::{json, render};
use ktask_core::{Journal, Phase, Project, RunOutcome, TaskId, TaskState, queue, read_evidence};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct TaskStatusInfo {
    id: String,
    title: String,
    state: String,
    protocol: Option<String>,
    phase: Option<String>,
    attempts: usize,
    started_at: Option<String>,
    ended_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatusSummary {
    queued: usize,
    preflight: usize,
    running: usize,
    remediating: usize,
    verifying: usize,
    publishing: usize,
    published_verified: usize,
    done: usize,
    acknowledged: usize,
    paused: usize,
    failed: usize,
    cancelled: usize,
}

#[derive(Debug, Serialize)]
struct StatusOutput {
    project: String,
    tasks: Vec<TaskStatusInfo>,
    summary: StatusSummary,
}

pub(crate) fn run(project: Option<Project>, json_output: bool) -> RunOutcome {
    let project = match project {
        Some(p) => p,
        None => {
            return RunOutcome::Usage {
                detail: "no project found".to_string(),
            };
        }
    };

    let tasks = match queue::load(&project) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error loading queue: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let journal = match Journal::open_for(&project) {
        Ok(j) => j,
        Err(e) => {
            render::progress(format_args!("error opening journal: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let states = match journal.all_states() {
        Ok(s) => s,
        Err(e) => {
            render::progress(format_args!("error loading states: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let mut task_statuses = Vec::new();
    let mut summary = StatusSummary {
        queued: 0,
        preflight: 0,
        running: 0,
        remediating: 0,
        verifying: 0,
        publishing: 0,
        published_verified: 0,
        done: 0,
        acknowledged: 0,
        paused: 0,
        failed: 0,
        cancelled: 0,
    };

    for task in &tasks {
        let state = states.get(&task.id);
        let (started_at, ended_at, attempts) = get_timing_info(&project, task.id, state);

        let state_name = state.map(|s| s.name().to_string()).unwrap_or_default();
        let phase_name = state.and_then(get_phase_name);

        update_summary(&mut summary, state);

        task_statuses.push(TaskStatusInfo {
            id: task.id.to_string(),
            title: task.title().to_string(),
            state: state_name,
            protocol: task.protocol_name(),
            phase: phase_name,
            attempts,
            started_at,
            ended_at,
        });
    }

    if json_output {
        let output = StatusOutput {
            project: project.id.to_string(),
            tasks: task_statuses,
            summary,
        };
        let _ = json::emit_json(&output);
    } else {
        for task_info in &task_statuses {
            let mut line = format!(
                "id={} state={} protocol={} phase={} attempts={}",
                task_info.id,
                task_info.state,
                task_info.protocol.as_deref().unwrap_or("none"),
                task_info.phase.as_deref().unwrap_or("none"),
                task_info.attempts
            );
            if let Some(started) = &task_info.started_at {
                let _ = std::fmt::write(&mut line, format_args!(" started={}", started));
            }
            if let Some(ended) = &task_info.ended_at {
                let _ = std::fmt::write(&mut line, format_args!(" ended={}", ended));
            }
            render::out(format_args!("{} {}", task_info.title, line));
        }

        render::out(format_args!(
            "summary: queued={} preflight={} running={} remediating={} verifying={} publishing={} published_verified={} done={} acknowledged={} paused={} failed={} cancelled={}",
            summary.queued,
            summary.preflight,
            summary.running,
            summary.remediating,
            summary.verifying,
            summary.publishing,
            summary.published_verified,
            summary.done,
            summary.acknowledged,
            summary.paused,
            summary.failed,
            summary.cancelled
        ));
    }

    RunOutcome::Drained
}

fn get_timing_info(
    project: &Project,
    task_id: TaskId,
    _state: Option<&TaskState>,
) -> (Option<String>, Option<String>, usize) {
    match read_evidence(project, task_id) {
        Ok(records) => {
            let count = records.len();
            let (started, ended) = if let Some(last) = records.last() {
                let started_str = last
                    .started
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok()
                    .map(|s| s.to_string());
                let ended_str = last.ended.as_ref().and_then(|t| {
                    t.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                        .map(|s| s.to_string())
                });
                (started_str, ended_str)
            } else {
                (None, None)
            };
            (started, ended, count)
        }
        Err(_) => (None, None, 0),
    }
}

fn get_phase_name(state: &TaskState) -> Option<String> {
    match state {
        TaskState::Running { phase, .. } | TaskState::Remediating { phase, .. } => {
            Some(phase_name(phase).to_string())
        }
        _ => None,
    }
}

fn phase_name(phase: &Phase) -> &'static str {
    match phase {
        Phase::Goal => "Goal",
        Phase::Scope => "Scope",
        Phase::AcceptanceTests => "AcceptanceTests",
        Phase::Implement => "Implement",
        Phase::Red => "Red",
        Phase::Green => "Green",
        Phase::Refactor => "Refactor",
        Phase::Review => "Review",
        Phase::Harden => "Harden",
        Phase::DoneCheck => "DoneCheck",
        Phase::Verify => "Verify",
        Phase::Publish => "Publish",
    }
}

fn update_summary(summary: &mut StatusSummary, state: Option<&TaskState>) {
    match state {
        Some(TaskState::Queued) => summary.queued += 1,
        Some(TaskState::Preflight) => summary.preflight += 1,
        Some(TaskState::Running { .. }) => summary.running += 1,
        Some(TaskState::Remediating { .. }) => summary.remediating += 1,
        Some(TaskState::Verifying { .. }) => summary.verifying += 1,
        Some(TaskState::Publishing { .. }) => summary.publishing += 1,
        Some(TaskState::PublishedVerified { .. }) => summary.published_verified += 1,
        Some(TaskState::Done) => summary.done += 1,
        Some(TaskState::Acknowledged { .. }) => summary.acknowledged += 1,
        Some(TaskState::Paused { .. }) => summary.paused += 1,
        Some(TaskState::Failed { .. }) => summary.failed += 1,
        Some(TaskState::Cancelled) => summary.cancelled += 1,
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn status_with_empty_queue() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), false);
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }

    #[test]
    fn status_json_output_format() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project.clone()), true);
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }

    #[test]
    fn status_no_project_fails() {
        let outcome = run(None, false);
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage error, got {outcome:?}"),
        }
    }
}

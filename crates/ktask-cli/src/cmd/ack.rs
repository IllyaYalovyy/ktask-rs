//! Ack command: pass a human gate.

use crate::render;
use ktask_core::{Journal, RunOutcome, TaskState, ids::TaskId, queue};
use std::collections::BTreeMap;
use time::OffsetDateTime;

pub(crate) fn run(project: Option<ktask_core::Project>, task: Option<String>) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Usage {
            detail: "no project found".to_string(),
        };
    };

    let tasks = match queue::load(&proj) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error loading queue: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let mut journal = match Journal::open_for(&proj) {
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

    // Find the task to acknowledge
    let task_id = match task {
        Some(t) => match t.parse::<u32>() {
            Ok(id) => TaskId::new(id),
            Err(_) => {
                return RunOutcome::Usage {
                    detail: format!("invalid task id: {t}"),
                };
            }
        },
        None => {
            // Find the first paused task with HumanGate reason
            match find_human_gate(&states) {
                Some(id) => id,
                None => {
                    return RunOutcome::Usage {
                        detail: "no human gate pending".to_string(),
                    };
                }
            }
        }
    };

    let state = states.get(&task_id);

    match state {
        Some(TaskState::Paused {
            reason: ktask_core::state::PauseReason::HumanGate,
            ..
        }) => {
            let now = OffsetDateTime::now_utc();
            let username = get_username();

            let event = ktask_core::EventKind::GateAcknowledged {
                by: username,
                at: now,
            };

            if let Err(e) = journal.append(Some(task_id), &event) {
                render::progress(format_args!("error journaling acknowledgment: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }

            render::out(format_args!(
                "id={} title={} state=done",
                task_id,
                tasks
                    .iter()
                    .find(|t| t.id == task_id)
                    .map(|t| t.title())
                    .unwrap_or("Unknown")
            ));

            RunOutcome::Drained
        }
        _ => RunOutcome::Usage {
            detail: format!("task {task_id} is not at a human gate"),
        },
    }
}

fn find_human_gate(states: &BTreeMap<TaskId, TaskState>) -> Option<TaskId> {
    for (id, state) in states {
        if matches!(
            state,
            TaskState::Paused {
                reason: ktask_core::state::PauseReason::HumanGate,
                ..
            }
        ) {
            return Some(*id);
        }
    }
    None
}

fn get_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn cli_ack_on_non_gated_task_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), Some("1".to_string()));
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }
}

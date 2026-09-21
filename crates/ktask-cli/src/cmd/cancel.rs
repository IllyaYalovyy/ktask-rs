//! Cancel command: mark a task cancelled.

use crate::render;
use ktask_core::{Journal, RunOutcome, ids::TaskId, queue};

pub(crate) fn run(project: Option<ktask_core::Project>, task: &str) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Usage {
            detail: "no project found".to_string(),
        };
    };

    // Parse the task ID
    let task_id = match task.parse::<u32>() {
        Ok(id) => TaskId::new(id),
        Err(_) => {
            return RunOutcome::Usage {
                detail: format!("invalid task id: {task}"),
            };
        }
    };

    // Load the queue to get task info
    let tasks = match queue::load(&proj) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error loading queue: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    // Load journal to check current state
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

    // Check if task exists and is in a cancellable state
    match states.get(&task_id) {
        Some(state) if state.is_terminal() => RunOutcome::Usage {
            detail: format!("task {task_id} is already in a terminal state"),
        },
        Some(_) => {
            // Task is in a non-terminal state, we can cancel it
            let event = ktask_core::EventKind::TaskCancelled {
                reason: "cancelled via CLI".to_string(),
            };

            if let Err(e) = journal.append(Some(task_id), &event) {
                render::progress(format_args!("error journaling cancellation: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }

            // Print the cancelled task
            let task_title = tasks
                .iter()
                .find(|t| t.id == task_id)
                .map_or("Unknown".to_string(), |t| t.title().to_string());

            render::out(format_args!(
                "id={} title={} state=cancelled",
                task_id, task_title
            ));
            RunOutcome::Drained
        }
        None => RunOutcome::Usage {
            detail: format!("task {task_id} not found"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn cancel_on_non_existent_task_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), "999");
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }
}

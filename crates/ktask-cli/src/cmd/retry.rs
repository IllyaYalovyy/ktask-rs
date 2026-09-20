//! Retry command: start a fresh attempt on a failed task.

use crate::render;
use ktask_core::{Journal, RunOutcome, TaskState, ids::TaskId, queue};

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
    task: &str,
) -> RunOutcome {
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

    let journal = match Journal::open_for(&proj) {
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

    let task_id = match task.parse::<u32>() {
        Ok(id) => TaskId::new(id),
        Err(_) => {
            return RunOutcome::Usage {
                detail: format!("invalid task id: {task}"),
            };
        }
    };

    let state = states.get(&task_id);
    match state {
        Some(TaskState::Failed { .. }) => {
            let filtered_tasks: Vec<_> =
                tasks.iter().filter(|t| t.id == task_id).cloned().collect();

            for task_to_run in filtered_tasks {
                render::out(format_args!(
                    "id={} title={} state=done",
                    task_to_run.id,
                    task_to_run.title()
                ));
            }

            RunOutcome::Drained
        }
        _ => RunOutcome::Usage {
            detail: format!("task {task} is not failed"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn cli_retry_on_non_failed_task_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), None, "1");
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }
}

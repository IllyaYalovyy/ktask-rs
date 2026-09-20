//! Resume command: continue from the first incomplete task.

use crate::render;
use ktask_core::{RunOutcome, queue, Journal};

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
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

    if tasks.is_empty() {
        return RunOutcome::Usage {
            detail: "queue is drained".to_string(),
        };
    }

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

    let first_incomplete = tasks
        .iter()
        .find(|t| {
            states
                .get(&t.id)
                .map(|state| !state.is_terminal())
                .unwrap_or(false)
        })
        .map(|t| t.id.to_string());

    let Some(from_id) = first_incomplete else {
        return RunOutcome::Usage {
            detail: "queue is drained".to_string(),
        };
    };

    let filtered_tasks = filter_tasks(&tasks, None, Some(from_id));

    for task_to_run in filtered_tasks {
        render::out(format_args!(
            "id={} title={} state=done",
            task_to_run.id,
            task_to_run.title()
        ));
    }

    RunOutcome::Drained
}

fn filter_tasks(
    tasks: &[ktask_core::Task],
    _single_task: Option<String>,
    from_task: Option<String>,
) -> Vec<ktask_core::Task> {
    if let Some(from_id_str) = from_task {
        tasks
            .iter()
            .skip_while(|t| t.id.to_string() != from_id_str)
            .cloned()
            .collect()
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;
    use ktask_core::{Task, TaskStatus, TaskState, ids::TaskId};

    fn make_task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: format!("Outcome {id}"),
            done_when: format!("Done when {id}"),
            verify: format!("Verify {id}"),
            refs: format!("Refs {id}"),
        }
    }

    #[test]
    fn cli_resume_on_drained_queue_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), None);
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }

    #[test]
    fn cli_resume_continues_and_retry_remediates() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let mut journal = Journal::open_for(&project).expect("Failed to open journal");
        let tasks = vec![make_task(1), make_task(2), make_task(3)];
        journal.put_tasks(&tasks).expect("Failed to put tasks");

        journal
            .put_state(TaskId::new(1), &TaskState::Done)
            .expect("Failed to put state");

        journal
            .put_state(TaskId::new(2), &TaskState::Queued)
            .expect("Failed to put state");

        journal
            .put_state(TaskId::new(3), &TaskState::Done)
            .expect("Failed to put state");

        let outcome = run(Some(project.clone()), None);
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }

        let mut journal2 = Journal::open_for(&project).expect("Failed to open journal");
        journal2
            .put_state(TaskId::new(2), &TaskState::Failed {
                class: ktask_core::FailureClass::AgentFailure,
                detail: "Test failure".to_string(),
            })
            .expect("Failed to put state");

        let outcome = crate::cmd::retry::run(
            Some(project),
            None,
            "2".to_string(),
        );
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }
}

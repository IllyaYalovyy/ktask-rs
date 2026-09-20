//! Run command: drain the queue in order.

use crate::render;
use ktask_core::{RunOutcome, queue};

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
    task: Option<String>,
    from: Option<String>,
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
        return RunOutcome::Drained;
    }

    let filtered_tasks = filter_tasks(&tasks, task, from);

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
    single_task: Option<String>,
    from_task: Option<String>,
) -> Vec<ktask_core::Task> {
    if let Some(task_id_str) = single_task {
        tasks
            .iter()
            .filter(|t| t.id.to_string() == task_id_str)
            .cloned()
            .collect()
    } else if let Some(from_id_str) = from_task {
        tasks
            .iter()
            .skip_while(|t| t.id.to_string() != from_id_str)
            .cloned()
            .collect()
    } else {
        tasks.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn cli_run_drains_the_queue() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), None, None, None);
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }
}

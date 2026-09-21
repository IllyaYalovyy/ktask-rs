//! Pause command: pause the running queue.

use crate::render;
use ktask_core::{RunOutcome, Journal, queue, control};
use std::collections::BTreeMap;

pub(crate) fn run(project: Option<ktask_core::Project>) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Usage {
            detail: "no project found".to_string(),
        };
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

    // Load journal to find active tasks
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

    // Find a task that is active (not terminal, not paused)
    let active_task = find_active_task(&states);

    match active_task {
        Some(task_id) => {
            // Write pause signal
            if let Err(e) = control::write_pause(&proj) {
                render::progress(format_args!("error writing pause signal: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }

            // Print the task we're pausing
            let task_title = tasks
                .iter()
                .find(|t| t.id == task_id)
                .map_or("Unknown".to_string(), |t| t.title().to_string());

            render::out(format_args!("id={} title={} action=paused", task_id, task_title));
            RunOutcome::Drained
        }
        None => RunOutcome::Usage {
            detail: "no running task to pause".to_string(),
        },
    }
}

fn find_active_task(states: &BTreeMap<ktask_core::TaskId, ktask_core::TaskState>) -> Option<ktask_core::TaskId> {
    for (id, state) in states {
        // A task is active if it's not terminal and not paused
        if !state.is_terminal() && !state.is_paused() {
            return Some(*id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn pause_with_no_active_task_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project));
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }
}

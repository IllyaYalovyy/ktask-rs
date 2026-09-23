//! `ktask-rs resume`: continues from the first task that is not done
//! (`docs/CONTRACT.md` section 3).
//!
//! It is `run --from` the first incomplete id, and nothing more: the queue's
//! ordering, the one-task-at-a-time rule and the exit codes are all `run`'s,
//! so this module only decides *where* to start and turns a queue with
//! nowhere left to start into a usage error. What it adds on top is a hint:
//! a task that has failed is not something `resume` can move past, and the
//! command that can is `retry`.
//!
//! Whether a task is complete is a pure function of its journaled state
//! ([`first_incomplete`]), tested without a journal.

use ktask_core::{Project, RunOutcome, Task, TaskId, TaskState};
use std::collections::BTreeMap;

use crate::cmd::run;
use crate::render;

/// Continues the queue from its first incomplete task, or reports it drained
/// as a usage error (exit 2): there is nothing to continue.
pub(crate) fn run(project: &Project, json: bool) -> RunOutcome {
    let (tasks, states) = match run::read_queue(project) {
        Ok(queue) => queue,
        Err(err) => {
            let detail = format!("resume: could not read the queue: {err}");
            render::progress(format_args!("error: {detail}"));
            return RunOutcome::CheckFailed { detail };
        }
    };
    let Some(first) = first_incomplete(&tasks, &states) else {
        return RunOutcome::Usage {
            detail: "resume: the queue is drained; there is nothing to resume".to_string(),
        };
    };

    let outcome = run::run(project, None, Some(first), json);
    if let RunOutcome::TaskFailed { task } = &outcome {
        render::progress(format_args!(
            "resume: `ktask-rs retry --task {task}` starts a fresh remediation attempt"
        ));
    }
    outcome
}

/// The first task in queue order that is not complete: not done,
/// acknowledged, cancelled or published and verified. A task with no
/// journaled state has not started, so it is incomplete.
fn first_incomplete(tasks: &[Task], states: &BTreeMap<TaskId, TaskState>) -> Option<TaskId> {
    tasks
        .iter()
        .find(|task| !is_complete(states.get(&task.id).unwrap_or(&TaskState::Queued)))
        .map(|task| task.id)
}

/// Whether `state` is one the queue has moved past: the same set
/// [`crate::cmd::run`] lets a successor start behind.
fn is_complete(state: &TaskState) -> bool {
    matches!(
        state,
        TaskState::PublishedVerified { .. }
            | TaskState::Done
            | TaskState::Acknowledged { .. }
            | TaskState::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{AttemptId, FailureClass, PauseReason, Phase, TaskStatus};
    use time::OffsetDateTime;

    fn tasks(count: u32) -> Vec<Task> {
        (1..=count)
            .map(|id| Task {
                id: TaskId::new(id),
                status: TaskStatus::Pending,
                body: format!("Task {id}"),
                outcome: "outcome".to_string(),
                done_when: "done".to_string(),
                verify: "true".to_string(),
                refs: "none".to_string(),
                protocol: None,
            })
            .collect()
    }

    fn states(list: &[(u32, TaskState)]) -> BTreeMap<TaskId, TaskState> {
        list.iter()
            .map(|(id, state)| (TaskId::new(*id), state.clone()))
            .collect()
    }

    fn published() -> TaskState {
        TaskState::PublishedVerified {
            commit: "abc".to_string(),
        }
    }

    #[test]
    fn the_first_task_that_is_not_complete_is_where_the_queue_resumes() {
        let states = states(&[(1, TaskState::Done), (2, TaskState::Queued)]);

        assert_eq!(
            first_incomplete(&tasks(3), &states),
            Some(TaskId::new(2)),
            "task 2 is the first that is not done, whatever follows it"
        );
    }

    #[test]
    fn a_task_with_no_journaled_state_has_not_started() {
        let states = states(&[(1, TaskState::Done)]);

        assert_eq!(first_incomplete(&tasks(2), &states), Some(TaskId::new(2)));
    }

    #[test]
    fn every_way_of_being_complete_is_skipped() {
        let states = states(&[
            (1, TaskState::Done),
            (2, published()),
            (
                3,
                TaskState::Acknowledged {
                    by: "someone".to_string(),
                    at: OffsetDateTime::UNIX_EPOCH,
                },
            ),
            (4, TaskState::Cancelled),
        ]);

        assert_eq!(first_incomplete(&tasks(4), &states), None);
        assert_eq!(first_incomplete(&tasks(5), &states), Some(TaskId::new(5)));
    }

    #[test]
    fn every_way_of_being_unfinished_is_where_the_queue_resumes() {
        let unfinished = [
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Implement,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Queued),
            },
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "boom".to_string(),
            },
        ];

        for state in unfinished {
            let states = states(&[(1, TaskState::Done), (2, state.clone())]);
            assert_eq!(
                first_incomplete(&tasks(3), &states),
                Some(TaskId::new(2)),
                "{} is not complete",
                state.name()
            );
        }
    }

    #[test]
    fn an_empty_queue_has_nothing_to_resume() {
        assert_eq!(first_incomplete(&[], &BTreeMap::new()), None);
    }
}

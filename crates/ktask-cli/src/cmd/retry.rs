//! `ktask-rs retry`: starts a fresh remediation attempt for a failed task
//! (`docs/CONTRACT.md` section 3).
//!
//! [`run()`] checks that the task really is [failed], then hands it to
//! [`Runner::retry_task`], which seeds a new provider session with the
//! failure bundle. Nothing here edits the task, the queue or the journal's
//! history: the one thing recorded is the `RetryStarted` event the core
//! journals when the attempt begins, and the failure it answers stays
//! exactly where it was.
//!
//! The retry runs that one task; it does not carry on down the queue. What
//! follows it is `resume`'s job, so a retry's exit code says only whether
//! *this* task is now done.
//!
//! [failed]: TaskState::Failed

use ktask_core::{Error, Project, RunOutcome, Runner, TaskId, TaskState};
use std::collections::BTreeMap;

use crate::cmd::run::{self, TaskResult};
use crate::render;

/// Retries `task`, exiting like `run`: 0 once the task is done, 1 if the
/// retry failed too, 2 if `task` is not in the queue or is not failed.
pub(crate) fn run(project: &Project, task: TaskId, json: bool) -> RunOutcome {
    let outcome = attempt(project, task, json);
    match &outcome {
        RunOutcome::Usage { .. } => {}
        RunOutcome::CheckFailed { detail } => render::progress(format_args!("error: {detail}")),
        other => render::progress(format_args!("retry: {}", run::describe(other))),
    }
    outcome
}

/// [`run()`] without its closing summary line.
fn attempt(project: &Project, task: TaskId, json: bool) -> RunOutcome {
    let (tasks, states) = match run::read_queue(project) {
        Ok(queue) => queue,
        Err(err) => {
            return RunOutcome::CheckFailed {
                detail: format!("retry: could not read the queue: {err}"),
            };
        }
    };
    let Some(chosen) = tasks.iter().find(|candidate| candidate.id == task) else {
        return RunOutcome::Usage {
            detail: format!("no task {task} in the queue"),
        };
    };
    if let Err(detail) = check_retryable(task, &states) {
        return RunOutcome::Usage { detail };
    }
    let mut runner = match Runner::new(project.clone()) {
        Ok(runner) => runner,
        Err(err) => {
            return RunOutcome::Usage {
                detail: format!("retry: {err}"),
            };
        }
    };

    let (result, pumped) =
        run::run_with_progress(&mut runner, json, |runner| runner.retry_task(chosen));
    if pumped.dropped > 0 {
        render::progress(format_args!(
            "retry: {} progress events were not shown; `ktask-rs status` has the final state \
             of every task",
            pumped.dropped
        ));
    }

    match result {
        Ok(_) => {
            render::progress(format_args!(
                "retry: task {task} is done; `ktask-rs resume` continues the queue"
            ));
            RunOutcome::Drained
        }
        Err(Error::InvalidTransition { .. }) => RunOutcome::Usage {
            detail: format!("retry: task {task} is not failed; only a failed task can be retried"),
        },
        Err(err) => {
            render::progress(format_args!("retry: task {task} failed again: {err}"));
            // A retry that never got as far as journaling a failure (a red
            // preflight, say) still owes its task a result line.
            if !pumped.reported.contains(&task) {
                run::emit_result(
                    &TaskResult {
                        task,
                        result: "failed",
                        detail: Some(err.to_string()),
                    },
                    json,
                );
            }
            RunOutcome::TaskFailed { task }
        }
    }
}

/// Whether `task` may be retried: it has a state, and that state is
/// [`TaskState::Failed`]. A task with no journaled state has not started, so
/// it is queued.
///
/// # Errors
///
/// Returns the usage-error text naming the task's actual state.
fn check_retryable(task: TaskId, states: &BTreeMap<TaskId, TaskState>) -> Result<(), String> {
    let state = states.get(&task).unwrap_or(&TaskState::Queued);
    match state {
        TaskState::Failed { .. } => Ok(()),
        other => Err(format!(
            "retry: task {task} is {}, not failed; only a failed task can be retried",
            other.name().to_lowercase()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{AttemptId, FailureClass, PauseReason, Phase};

    fn states(list: &[(u32, TaskState)]) -> BTreeMap<TaskId, TaskState> {
        list.iter()
            .map(|(id, state)| (TaskId::new(*id), state.clone()))
            .collect()
    }

    #[test]
    fn a_failed_task_may_be_retried() {
        let states = states(&[(
            2,
            TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: "red".to_string(),
            },
        )]);

        assert_eq!(check_retryable(TaskId::new(2), &states), Ok(()));
    }

    #[test]
    fn a_task_that_is_not_failed_is_refused_naming_its_state() {
        let refused = [
            (TaskState::Queued, "queued"),
            (TaskState::Done, "done"),
            (TaskState::Cancelled, "cancelled"),
            (
                TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
                "running",
            ),
            (
                TaskState::Paused {
                    reason: PauseReason::Input,
                    resume_to: Box::new(TaskState::Queued),
                },
                "paused",
            ),
        ];

        for (state, name) in refused {
            let states = states(&[(1, state)]);
            let detail = check_retryable(TaskId::new(1), &states)
                .expect_err("only a failed task can be retried");
            assert_eq!(
                detail,
                format!("retry: task 1 is {name}, not failed; only a failed task can be retried")
            );
        }
    }

    #[test]
    fn a_task_with_no_journaled_state_is_queued_and_so_refused() {
        let detail = check_retryable(TaskId::new(4), &BTreeMap::new())
            .expect_err("a task that never started has not failed");

        assert!(detail.contains("is queued"), "got {detail}");
    }
}

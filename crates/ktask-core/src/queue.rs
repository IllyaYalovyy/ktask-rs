//! Queue selection: determining the next runnable task.

use std::collections::BTreeMap;

use crate::ids::TaskId;
use crate::state::{TaskState, check_one_active, check_predecessor};
use crate::{Result, Task};

/// Select the next runnable task from the queue.
///
/// Returns the lowest task id whose state is `Queued` and whose predecessors
/// all satisfy `TaskState::is_terminal()`.
///
/// Returns `Ok(None)` when:
/// - Any task is paused
/// - Any task is at a human gate (Acknowledged)
/// - Any task has failed
/// - The queue is drained (no Queued tasks with satisfied predecessors)
///
/// A task absent from `states` is not treated as `Queued` — only tasks
/// explicitly recorded in the state map are considered.
///
/// # Errors
///
/// Returns `Err` if `check_one_active` or `check_predecessor` fail, indicating
/// a policy violation that must be resolved before selection can proceed.
pub fn next_runnable(
    tasks: &[Task],
    states: &BTreeMap<TaskId, TaskState>,
) -> Result<Option<TaskId>> {
    // If any task is paused, return None (queue is blocked).
    if states.values().any(TaskState::is_paused) {
        return Ok(None);
    }

    // If any task is failed or at a human gate (Acknowledged), return None (queue is blocked).
    if states.values().any(|state| {
        matches!(
            state,
            TaskState::Failed { .. } | TaskState::Acknowledged { .. }
        )
    }) {
        return Ok(None);
    }

    // Enforce that at most one task is active.
    check_one_active(states)?;

    // Find the lowest task id that is Queued and whose predecessors are all terminal.
    for task in tasks {
        if let Some(TaskState::Queued) = states.get(&task.id) {
            check_predecessor(states, task.id)?;
            return Ok(Some(task.id));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::FailureClass;
    use crate::ids::AttemptId;
    use crate::state::Phase;

    fn task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: crate::task::TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: format!("Outcome {id}"),
            done_when: format!("Done when {id}"),
            verify: format!("Verify {id}"),
            refs: format!("Refs {id}"),
        }
    }

    #[test]
    fn next_runnable_drained_queue() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Done);
        states.insert(TaskId::new(3), TaskState::Done);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn next_runnable_blocking_failure() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(
            TaskId::new(1),
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "Failed".to_string(),
            },
        );
        states.insert(TaskId::new(2), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn next_runnable_pending_human_gate() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(
            TaskId::new(1),
            TaskState::Acknowledged {
                by: "test_user".to_string(),
                at: time::OffsetDateTime::now_utc(),
            },
        );
        states.insert(TaskId::new(2), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn next_runnable_ordinary_next_task() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(TaskId::new(2)));
    }

    #[test]
    fn next_runnable_returns_lowest_queued() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(TaskId::new(2)));
    }

    #[test]
    fn next_runnable_respects_predecessor_constraint() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        // Task 1 is still running, so task 2 cannot start
        states.insert(
            TaskId::new(1),
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
        );
        states.insert(TaskId::new(2), TaskState::Queued);
        states.insert(TaskId::new(3), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_err());
    }

    #[test]
    fn next_runnable_skips_non_queued_tasks() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Preflight);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn next_runnable_paused_state_blocks_queue() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(
            TaskId::new(1),
            TaskState::Paused {
                reason: crate::state::PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                }),
            },
        );
        states.insert(TaskId::new(2), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn next_runnable_multiple_predecessors_all_terminal() {
        let tasks = vec![task(1), task(2), task(3), task(4)];
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Cancelled);
        states.insert(TaskId::new(3), TaskState::Queued);

        let result = next_runnable(&tasks, &states);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(TaskId::new(3)));
    }
}

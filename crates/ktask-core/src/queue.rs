//! `next_runnable`: which task the runner should pick up next.
//!
//! A pure function of queue state, per `VISION.md` §6's task lifecycle and
//! §3's invariants 1 and 2. It reuses [`check_one_active`] and
//! [`check_predecessor`] rather than reimplementing either, so the queue's
//! notion of "runnable" can never drift from the invariants those functions
//! already enforce.

use crate::{Result, Task, TaskId, TaskState, check_one_active, check_predecessor};
use std::collections::BTreeMap;

#[cfg(test)]
use crate::{EventKind, apply};

/// Picks the next task the runner should start, or `Ok(None)` if nothing is
/// runnable right now.
///
/// A task qualifies when its own state is [`TaskState::Queued`] and
/// [`check_predecessor`] accepts it — every task ahead of it in `tasks` has
/// reached a settled state. Among qualifying tasks, the lowest id wins,
/// matching queue order. A task with no entry in `states` has not yet had
/// its `TaskQueued` event recorded, so it is not treated as `Queued` and is
/// never selected.
///
/// The whole queue is idle — `Ok(None)` — the moment any task anywhere is
/// paused (including sitting at an unacknowledged human gate, since that is
/// represented as a `Paused` state) or has failed: either condition means a
/// human or the recovery policy must act before the queue may proceed, and
/// `check_predecessor` would refuse every task behind it anyway.
///
/// # Errors
///
/// Propagates [`Error::Policy`](crate::Error::Policy) from
/// [`check_one_active`] if more than one task in `states` is active, since
/// that is an invariant violation in `states` itself rather than a normal
/// "nothing to run" outcome.
pub fn next_runnable(
    tasks: &[Task],
    states: &BTreeMap<TaskId, TaskState>,
) -> Result<Option<TaskId>> {
    check_one_active(states)?;

    let blocked = states
        .values()
        .any(|state| state.is_paused() || matches!(state, TaskState::Failed { .. }));
    if blocked {
        return Ok(None);
    }

    let queued_ids = tasks
        .iter()
        .map(|task| task.id)
        .filter(|id| matches!(states.get(id), Some(TaskState::Queued)));

    for id in queued_ids {
        if check_predecessor(states, id).is_ok() {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, PauseReason, Phase, TaskStatus};

    fn task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "true".to_string(),
            refs: "none".to_string(),
        }
    }

    #[test]
    fn a_drained_queue_has_nothing_runnable() {
        let tasks = vec![task(1), task(2)];
        let states = BTreeMap::from([
            (TaskId::new(1), TaskState::Done),
            (TaskId::new(2), TaskState::Cancelled),
        ]);

        assert_eq!(next_runnable(&tasks, &states).unwrap(), None);
    }

    #[test]
    fn a_blocking_failure_stops_every_task_behind_it() {
        let tasks = vec![task(1), task(2)];
        let states = BTreeMap::from([
            (
                TaskId::new(1),
                TaskState::Failed {
                    class: crate::FailureClass::VerificationFailure,
                    detail: "tests failed".to_string(),
                },
            ),
            (TaskId::new(2), TaskState::Queued),
        ]);

        assert_eq!(next_runnable(&tasks, &states).unwrap(), None);
    }

    #[test]
    fn a_pending_human_gate_stops_the_queue() {
        let tasks = vec![task(1), task(2)];
        let states = BTreeMap::from([
            (
                TaskId::new(1),
                TaskState::Paused {
                    reason: PauseReason::HumanGate,
                    resume_to: Box::new(TaskState::Queued),
                },
            ),
            (TaskId::new(2), TaskState::Queued),
        ]);

        assert_eq!(next_runnable(&tasks, &states).unwrap(), None);
    }

    #[test]
    fn an_ordinary_next_task_is_the_lowest_queued_id_with_settled_predecessors() {
        let tasks = vec![task(1), task(2), task(3)];
        let states = BTreeMap::from([
            (TaskId::new(1), TaskState::Done),
            (TaskId::new(2), TaskState::Queued),
            (TaskId::new(3), TaskState::Queued),
        ]);

        assert_eq!(
            next_runnable(&tasks, &states).unwrap(),
            Some(TaskId::new(2))
        );
    }

    #[test]
    fn a_task_absent_from_states_is_not_treated_as_queued() {
        let tasks = vec![task(1), task(2)];
        let states = BTreeMap::from([(TaskId::new(1), TaskState::Done)]);

        assert_eq!(next_runnable(&tasks, &states).unwrap(), None);
    }

    #[test]
    fn a_published_verified_predecessor_unblocks_its_successor() {
        let tasks = vec![task(1), task(2)];
        let states = BTreeMap::from([
            (
                TaskId::new(1),
                TaskState::PublishedVerified {
                    commit: "abc123".to_string(),
                },
            ),
            (TaskId::new(2), TaskState::Queued),
        ]);

        assert_eq!(
            next_runnable(&tasks, &states).unwrap(),
            Some(TaskId::new(2))
        );
    }

    #[test]
    fn more_than_one_active_task_is_an_error_not_none() {
        let tasks = vec![task(1), task(2)];
        let states = BTreeMap::from([
            (
                TaskId::new(1),
                TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
            ),
            (
                TaskId::new(2),
                TaskState::Verifying {
                    attempt: AttemptId::new(1),
                },
            ),
        ]);

        assert!(next_runnable(&tasks, &states).is_err());
    }

    #[test]
    fn an_empty_queue_has_nothing_runnable() {
        assert_eq!(next_runnable(&[], &BTreeMap::new()).unwrap(), None);
    }

    #[test]
    fn a_gate_stops_the_queue_until_acknowledged_then_the_rest_run() {
        let tasks = vec![task(1), task(2), task(3)];
        let mut states = BTreeMap::from([
            (TaskId::new(1), TaskState::Queued),
            (TaskId::new(2), TaskState::Queued),
            (TaskId::new(3), TaskState::Queued),
        ]);

        // Task 1 has no gate: it runs to completion normally.
        assert_eq!(
            next_runnable(&tasks, &states).unwrap(),
            Some(TaskId::new(1))
        );
        states.insert(TaskId::new(1), TaskState::Done);

        // Task 2 is a human gate: it goes straight to `Paused`, never to
        // `Running` — a gate entry is never handed to a provider.
        assert_eq!(
            next_runnable(&tasks, &states).unwrap(),
            Some(TaskId::new(2))
        );
        states.insert(
            TaskId::new(2),
            TaskState::Paused {
                reason: PauseReason::HumanGate,
                resume_to: Box::new(TaskState::Queued),
            },
        );

        // The queue stops: task 3 is blocked behind the unacknowledged gate.
        assert_eq!(next_runnable(&tasks, &states).unwrap(), None);

        // `ack` resolves the gate to `Acknowledged`.
        let acknowledged = apply(
            &states[&TaskId::new(2)],
            &EventKind::GateAcknowledged {
                by: "alice".to_string(),
                at: time::OffsetDateTime::UNIX_EPOCH,
            },
        )
        .expect("a HumanGate pause accepts GateAcknowledged");
        assert_eq!(
            acknowledged,
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: time::OffsetDateTime::UNIX_EPOCH,
            }
        );
        states.insert(TaskId::new(2), acknowledged);

        // With the gate acknowledged, task 3 becomes runnable.
        assert_eq!(
            next_runnable(&tasks, &states).unwrap(),
            Some(TaskId::new(3))
        );
    }
}

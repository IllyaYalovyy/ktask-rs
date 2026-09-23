//! `ktask-rs ack`: passes a human gate (`docs/CONTRACT.md` section 3).
//!
//! A gate "produces no commit, and completes only by human acknowledgement,
//! which is recorded with who and when" (`VISION.md` §3 invariant 7). This
//! records exactly that — a `GateAcknowledged` event carrying the
//! acknowledger and the instant — and stops. It does not run anything: the
//! queue stays paused until `resume` continues it, so passing a gate and
//! starting the work behind it are two decisions, not one.
//!
//! Which gate is a pure function of the journaled states ([`pending_gate`]);
//! the event is checked against [`ktask_core::apply`] before it is appended,
//! so a refused acknowledgement never reaches the journal.

use ktask_core::{
    Config, EventKind, Journal, PauseReason, Project, RunOutcome, Task, TaskId, TaskState, apply,
};
use std::collections::BTreeMap;
use std::env::{self, VarError};
use time::OffsetDateTime;

use crate::cmd::run::read_queue;
use crate::render;

/// Acknowledges the gate `task` names, or the first pending gate in queue
/// order when it is omitted. Exits 0 once the gate is journaled as
/// acknowledged; exits 2 when no gate is pending, or `task` is not one.
pub(crate) fn run(project: &Project, _config: &Config, task: Option<TaskId>) -> RunOutcome {
    acknowledge(
        project,
        task,
        &|key| env::var(key),
        OffsetDateTime::now_utc(),
    )
}

/// [`run`] with the environment and the clock passed in, so a test needs
/// neither the real process environment nor the real time.
fn acknowledge(
    project: &Project,
    task: Option<TaskId>,
    env_var: &dyn Fn(&str) -> Result<String, VarError>,
    at: OffsetDateTime,
) -> RunOutcome {
    let (tasks, states) = match read_queue(project) {
        Ok(queue) => queue,
        Err(err) => {
            let detail = format!("ack: could not read the queue: {err}");
            render::progress(format_args!("error: {detail}"));
            return RunOutcome::CheckFailed { detail };
        }
    };
    let id = match pending_gate(&tasks, &states, task) {
        Ok(id) => id,
        Err(detail) => return RunOutcome::Usage { detail },
    };

    let by = who(env_var);
    let event = EventKind::GateAcknowledged { by: by.clone(), at };
    // `pending_gate` only names a task in a `HumanGate` pause, the one state
    // that accepts this event; checking anyway keeps that a fact `apply`
    // states rather than one this function assumes.
    let state = states.get(&id).unwrap_or(&TaskState::Queued);
    if let Err(err) = apply(state, &event) {
        return RunOutcome::Usage {
            detail: format!("ack: task {id} cannot be acknowledged: {err}"),
        };
    }
    let appended =
        Journal::open_for(project).and_then(|mut journal| journal.append(Some(id), &event));
    if let Err(err) = appended {
        let detail = format!("ack: could not record the acknowledgement of task {id}: {err}");
        render::progress(format_args!("error: {detail}"));
        return RunOutcome::CheckFailed { detail };
    }

    render::out(format_args!("task {id} acknowledged by {by}"));
    render::progress(format_args!(
        "ack: the queue stays paused; `ktask-rs resume` continues it"
    ));
    RunOutcome::Drained
}

/// The task `ack` acts on: `task` if it is at a human gate, else — when
/// `task` is omitted — the first task in queue order that is.
///
/// # Errors
///
/// Returns the usage-error text: `task` is not in the queue or is not at a
/// human gate (naming the state it is in), or no gate is pending at all.
fn pending_gate(
    tasks: &[Task],
    states: &BTreeMap<TaskId, TaskState>,
    task: Option<TaskId>,
) -> Result<TaskId, String> {
    let at_gate = |id: TaskId| {
        matches!(
            states.get(&id),
            Some(TaskState::Paused {
                reason: PauseReason::HumanGate,
                ..
            })
        )
    };

    let Some(id) = task else {
        return tasks
            .iter()
            .map(|candidate| candidate.id)
            .find(|id| at_gate(*id))
            .ok_or_else(|| "ack: no human gate is pending".to_string());
    };

    if !tasks.iter().any(|candidate| candidate.id == id) {
        return Err(format!("no task {id} in the queue"));
    }
    if at_gate(id) {
        return Ok(id);
    }
    let state = states.get(&id).unwrap_or(&TaskState::Queued);
    Err(format!(
        "ack: task {id} is {}, not at a human gate",
        state.name().to_lowercase()
    ))
}

/// Who is acknowledging: `$USER`, else `$LOGNAME`, else `unknown`. An empty
/// value counts as unset. The name is a record of who said so, not a
/// credential — nothing here authenticates it.
fn who(env_var: &dyn Fn(&str) -> Result<String, VarError>) -> String {
    ["USER", "LOGNAME"]
        .into_iter()
        .filter_map(|key| env_var(key).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::run::read_queue;
    use ktask_core::{
        AttemptId, EventKind, Journal, PauseReason, Phase, Task, TaskState, TaskStatus,
    };
    use std::collections::BTreeMap;
    use std::env::VarError;
    use time::OffsetDateTime;

    fn task(id: u32, status: TaskStatus) -> Task {
        Task {
            id: TaskId::new(id),
            status,
            body: format!("Task {id}"),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "true".to_string(),
            refs: "none".to_string(),
            protocol: None,
        }
    }

    fn gate_pause() -> TaskState {
        TaskState::Paused {
            reason: PauseReason::HumanGate,
            resume_to: Box::new(TaskState::Queued),
        }
    }

    fn states(list: &[(u32, TaskState)]) -> BTreeMap<TaskId, TaskState> {
        list.iter()
            .map(|(id, state)| (TaskId::new(*id), state.clone()))
            .collect()
    }

    fn env_of(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Result<String, VarError> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
                .ok_or(VarError::NotPresent)
        }
    }

    /// A project whose journal holds `tasks`, each with the pause events that
    /// leave it in the given state: `HumanGate` parks it at a gate, `Input`
    /// at a question.
    fn project_with(dir: &tempfile::TempDir, tasks: &[(Task, Option<PauseReason>)]) -> Project {
        let project = Project {
            root: dir.path().join("repo"),
            id: "ack-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        let mut journal = Journal::open_for(&project).expect("open journal");
        let all: Vec<Task> = tasks.iter().map(|(task, _)| task.clone()).collect();
        journal.put_tasks(&all).expect("put tasks");
        for (task, pause) in tasks {
            if let Some(reason) = pause {
                journal
                    .append(
                        Some(task.id),
                        &EventKind::Paused {
                            reason: reason.clone(),
                        },
                    )
                    .expect("append pause");
            }
        }
        project
    }

    fn at() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    // -- pending_gate ---------------------------------------------------------

    #[test]
    fn without_a_task_the_first_pending_gate_in_queue_order_is_chosen() {
        let tasks = [
            task(1, TaskStatus::Pending),
            task(2, TaskStatus::HumanGate),
            task(3, TaskStatus::HumanGate),
        ];
        let states = states(&[(1, TaskState::Done), (2, gate_pause()), (3, gate_pause())]);

        assert_eq!(pending_gate(&tasks, &states, None), Ok(TaskId::new(2)));
    }

    #[test]
    fn a_named_task_is_chosen_even_when_an_earlier_gate_is_pending() {
        let tasks = [
            task(2, TaskStatus::HumanGate),
            task(3, TaskStatus::HumanGate),
        ];
        let states = states(&[(2, gate_pause()), (3, gate_pause())]);

        assert_eq!(
            pending_gate(&tasks, &states, Some(TaskId::new(3))),
            Ok(TaskId::new(3))
        );
    }

    #[test]
    fn no_pending_gate_is_an_error_naming_the_absence() {
        let tasks = [task(1, TaskStatus::Pending)];
        let states = states(&[(1, TaskState::Queued)]);

        assert_eq!(
            pending_gate(&tasks, &states, None),
            Err("ack: no human gate is pending".to_string())
        );
    }

    #[test]
    fn a_task_paused_for_another_reason_is_not_a_pending_gate() {
        let tasks = [task(1, TaskStatus::Pending)];
        let paused = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }),
        };
        let states = states(&[(1, paused)]);

        assert_eq!(
            pending_gate(&tasks, &states, None),
            Err("ack: no human gate is pending".to_string())
        );
        assert_eq!(
            pending_gate(&tasks, &states, Some(TaskId::new(1))),
            Err("ack: task 1 is paused, not at a human gate".to_string())
        );
    }

    #[test]
    fn a_named_task_that_is_not_at_a_gate_is_refused_naming_its_state() {
        let tasks = [task(1, TaskStatus::Pending), task(2, TaskStatus::HumanGate)];
        let states = states(&[(1, TaskState::Done), (2, TaskState::Queued)]);

        assert_eq!(
            pending_gate(&tasks, &states, Some(TaskId::new(1))),
            Err("ack: task 1 is done, not at a human gate".to_string())
        );
        assert_eq!(
            pending_gate(&tasks, &states, Some(TaskId::new(2))),
            Err("ack: task 2 is queued, not at a human gate".to_string())
        );
    }

    #[test]
    fn a_task_not_in_the_queue_is_refused() {
        assert_eq!(
            pending_gate(&[], &BTreeMap::new(), Some(TaskId::new(9))),
            Err("no task 9 in the queue".to_string())
        );
    }

    // -- who ------------------------------------------------------------------

    #[test]
    fn the_acknowledger_is_user_then_logname_then_unknown() {
        assert_eq!(
            who(&env_of(&[("USER", "alice"), ("LOGNAME", "bob")])),
            "alice"
        );
        assert_eq!(who(&env_of(&[("LOGNAME", "bob")])), "bob");
        assert_eq!(who(&env_of(&[])), "unknown");
        assert_eq!(who(&env_of(&[("USER", "")])), "unknown");
    }

    // -- acknowledge ----------------------------------------------------------

    #[test]
    fn acknowledging_a_gate_journals_who_and_when_and_completes_the_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(
            &dir,
            &[(task(1, TaskStatus::HumanGate), Some(PauseReason::HumanGate))],
        );

        let outcome = acknowledge(&project, None, &env_of(&[("USER", "alice")]), at());

        assert_eq!(outcome, RunOutcome::Drained);
        let (_, states) = read_queue(&project).expect("read queue");
        assert_eq!(
            states[&TaskId::new(1)],
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: at(),
            }
        );
        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(TaskId::new(1)).expect("events");
        assert_eq!(
            events.last().map(|event| event.kind.clone()),
            Some(EventKind::GateAcknowledged {
                by: "alice".to_string(),
                at: at(),
            })
        );
    }

    #[test]
    fn acknowledging_passes_only_the_first_pending_gate_and_leaves_the_rest_paused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(
            &dir,
            &[
                (task(1, TaskStatus::HumanGate), Some(PauseReason::HumanGate)),
                (task(2, TaskStatus::HumanGate), Some(PauseReason::HumanGate)),
            ],
        );

        let outcome = acknowledge(&project, None, &env_of(&[("USER", "alice")]), at());

        assert_eq!(outcome, RunOutcome::Drained);
        let (_, states) = read_queue(&project).expect("read queue");
        assert!(matches!(
            states[&TaskId::new(1)],
            TaskState::Acknowledged { .. }
        ));
        assert_eq!(states[&TaskId::new(2)], gate_pause());
    }

    #[test]
    fn a_named_gate_is_the_one_acknowledged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(
            &dir,
            &[
                (task(1, TaskStatus::HumanGate), Some(PauseReason::HumanGate)),
                (task(2, TaskStatus::HumanGate), Some(PauseReason::HumanGate)),
            ],
        );

        let outcome = acknowledge(
            &project,
            Some(TaskId::new(2)),
            &env_of(&[("USER", "alice")]),
            at(),
        );

        assert_eq!(outcome, RunOutcome::Drained);
        let (_, states) = read_queue(&project).expect("read queue");
        assert_eq!(states[&TaskId::new(1)], gate_pause());
        assert!(matches!(
            states[&TaskId::new(2)],
            TaskState::Acknowledged { .. }
        ));
    }

    #[test]
    fn when_no_gate_is_pending_it_is_a_usage_error_and_the_journal_is_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[(task(1, TaskStatus::Pending), None)]);
        let before = Journal::open_for(&project)
            .expect("open journal")
            .events()
            .expect("events")
            .len();

        let outcome = acknowledge(&project, None, &env_of(&[("USER", "alice")]), at());

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "ack: no human gate is pending".to_string()
            }
        );
        let after = Journal::open_for(&project)
            .expect("open journal")
            .events()
            .expect("events")
            .len();
        assert_eq!(before, after, "nothing may be journaled for a refused ack");
    }

    #[test]
    fn a_task_waiting_for_input_is_not_acknowledgeable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(
            &dir,
            &[(task(1, TaskStatus::Pending), Some(PauseReason::Input))],
        );

        let outcome = acknowledge(&project, Some(TaskId::new(1)), &env_of(&[]), at());

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "ack: task 1 is paused, not at a human gate".to_string()
            }
        );
    }

    #[test]
    fn an_unreadable_queue_is_a_check_failure_not_a_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `state_dir` is a file, so the journal cannot be opened beneath it.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "").expect("write blocker");
        let project = Project {
            root: dir.path().join("repo"),
            id: "ack-broken".to_string(),
            state_dir: blocker,
        };

        let outcome = acknowledge(&project, None, &env_of(&[]), at());

        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
    }
}

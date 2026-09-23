//! `ktask-rs pause` / `ktask-rs interrupt` / `ktask-rs cancel`: control of a
//! run in progress, from any terminal (`docs/CONTRACT.md` section 3).
//!
//! A run is a supervisor in another process, so `pause` and `interrupt` do
//! not act on anything themselves: each drops a request into the project's
//! control directory ([`ktask_core::send`]) that the supervisor finds at its
//! next boundary or between two chunks of the agent's output, and journals
//! as `Paused`, `Interrupted` or `TaskCancelled` when it acts on it.
//! `interrupt` (and a `cancel` of the running task) then waits, bounded, for
//! that to happen, so a script can follow it with `resume` without racing
//! the supervisor it just stopped.
//!
//! `cancel` on a task that is *not* running has no supervisor to ask: it
//! journals `TaskCancelled` itself, checked against [`ktask_core::apply`]
//! first like every other command that writes an event, so the queue may
//! proceed past the task at once.
//!
//! Each command exits 2 when nothing is in a state it applies to: no task is
//! running (`pause`, `interrupt`), the running task's supervisor has exited
//! and left its record behind (there is no one to ask, and `resume`
//! reconciles it first), or the task is one `cancel` cannot help (finished,
//! or mid-publication). Whether a task counts as running is a pure function
//! of its journaled state ([`in_flight`], [`plan_cancel`]).

use ktask_core::{
    Config, EventKind, Journal, Project, Request, RunOutcome, Task, TaskId, TaskState, apply, send,
    supervisor_alive,
};
use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

use crate::cmd::run::read_queue;
use crate::render;

/// How long `interrupt` and `cancel` wait for the supervisor to act on the
/// request before reporting that it has not yet.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the journal is re-read while waiting.
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// The reason journaled with a `cancel` that acts directly on state. A
/// supervisor honoring the request journals its own, in
/// `Runner::stop_if_requested`.
const CANCEL_REASON: &str = "cancelled by `ktask-rs cancel`";

/// Stops the queue once the running task reaches a safe boundary: the
/// running task finishes, and the next one is parked instead of started
/// ([`ktask_core::Runner::run_queue`]). `run` or `resume` lifts the pause.
///
/// Exits 0 once the request is sent; 2 when no task is running.
pub(crate) fn pause(project: &Project, _config: &Config) -> RunOutcome {
    let running = match running_task("pause", project) {
        Ok(running) => running,
        Err(outcome) => return outcome,
    };
    if let Err(err) = send(project, Request::Pause) {
        return check_failed(format!("pause: could not send the request: {err}"));
    }
    render::out(format_args!(
        "pause requested: the queue stops after task {running}"
    ));
    RunOutcome::Drained
}

/// Terminates the running attempt now, leaving it durably interrupted and
/// resumable, and waits for the supervisor to have done so.
///
/// Exits 0 once the task is journaled as interrupted; 2 when no task is
/// running, or it stopped some other way first; 1 when the supervisor has
/// not acted within [`SETTLE_TIMEOUT`] (the request stays pending).
pub(crate) fn interrupt(project: &Project, _config: &Config) -> RunOutcome {
    interrupt_within(project, SETTLE_TIMEOUT)
}

/// [`interrupt`] with the wait passed in, so a test need not sit through
/// [`SETTLE_TIMEOUT`].
fn interrupt_within(project: &Project, timeout: Duration) -> RunOutcome {
    let running = match running_task("interrupt", project) {
        Ok(running) => running,
        Err(outcome) => return outcome,
    };
    if let Err(err) = send(project, Request::Interrupt) {
        return check_failed(format!("interrupt: could not send the request: {err}"));
    }
    render::progress(format_args!(
        "interrupt: asked the supervisor to stop task {running}"
    ));

    match settle("interrupt", project, running, timeout) {
        Ok(TaskState::Paused { .. }) => {
            render::out(format_args!("task {running} interrupted"));
            RunOutcome::Drained
        }
        Ok(state) => RunOutcome::Usage {
            detail: format!(
                "interrupt: task {running} is {}; it stopped before the interrupt took effect",
                state.name().to_lowercase()
            ),
        },
        Err(outcome) => outcome,
    }
}

/// Marks `task` cancelled so the queue may proceed past it.
///
/// A task that is not running is cancelled directly, in the journal; a
/// running one is cancelled by its supervisor, which terminates the attempt
/// first, and this waits for that. Exits 0 once the task is cancelled; 2
/// when `task` is not in the queue or is in a state cancelling cannot help;
/// 1 when a supervisor has not acted within [`SETTLE_TIMEOUT`].
pub(crate) fn cancel(project: &Project, _config: &Config, task: TaskId) -> RunOutcome {
    cancel_within(project, task, SETTLE_TIMEOUT)
}

/// [`cancel`] with the wait passed in.
fn cancel_within(project: &Project, task: TaskId, timeout: Duration) -> RunOutcome {
    let (tasks, states) = match read_queue(project) {
        Ok(queue) => queue,
        Err(err) => return check_failed(format!("cancel: could not read the queue: {err}")),
    };
    if !tasks.iter().any(|candidate| candidate.id == task) {
        return RunOutcome::Usage {
            detail: format!("no task {task} in the queue"),
        };
    }
    let state = states.get(&task).unwrap_or(&TaskState::Queued);

    match plan_cancel(state) {
        CancelPlan::Refuse(why) => RunOutcome::Usage {
            detail: format!("cancel: task {task} {why}"),
        },
        CancelPlan::Directly => cancel_directly(project, task, state),
        CancelPlan::ThroughSupervisor => {
            match supervisor_alive(project, task) {
                Ok(true) => {}
                Ok(false) => return orphaned("cancel", task, state),
                Err(err) => return check_failed(format!("cancel: {err}")),
            }
            if let Err(err) = send(project, Request::Cancel(task)) {
                return check_failed(format!("cancel: could not send the request: {err}"));
            }
            render::progress(format_args!(
                "cancel: asked the supervisor to stop task {task}"
            ));
            match settle("cancel", project, task, timeout) {
                Ok(TaskState::Cancelled) => {
                    render::out(format_args!("task {task} cancelled"));
                    RunOutcome::Drained
                }
                Ok(state) => RunOutcome::Usage {
                    detail: format!(
                        "cancel: task {task} is {}; it stopped before the cancel took effect",
                        state.name().to_lowercase()
                    ),
                },
                Err(outcome) => outcome,
            }
        }
    }
}

/// Journals `TaskCancelled` for `task`, which is in `state`, having checked
/// that the transition is legal.
fn cancel_directly(project: &Project, task: TaskId, state: &TaskState) -> RunOutcome {
    let event = EventKind::TaskCancelled {
        reason: CANCEL_REASON.to_string(),
    };
    if let Err(err) = apply(state, &event) {
        return RunOutcome::Usage {
            detail: format!("cancel: task {task} cannot be cancelled: {err}"),
        };
    }
    let appended =
        Journal::open_for(project).and_then(|mut journal| journal.append(Some(task), &event));
    if let Err(err) = appended {
        return check_failed(format!(
            "cancel: could not record the cancellation of task {task}: {err}"
        ));
    }
    render::out(format_args!("task {task} cancelled"));
    RunOutcome::Drained
}

/// What `cancel` does for a task in a given state.
#[derive(Debug, PartialEq, Eq)]
enum CancelPlan {
    /// Nothing is running it: journal the cancellation here.
    Directly,
    /// A supervisor is running it: ask that supervisor to.
    ThroughSupervisor,
    /// Cancelling would not help; the text completes "task N ...".
    Refuse(String),
}

/// How `cancel` treats a task in `state`.
///
/// `Queued`, `Paused` and `Failed` tasks are the ones that hold the queue up
/// with nothing running: they are cancelled directly. A running task is
/// cancelled by the supervisor that is running it. A task that is
/// publishing is refused: publication is a commit and a push that must end
/// verified or not at all, and no boundary inside it can be interrupted
/// (`state::apply` has no `TaskCancelled` arm for it either). A finished
/// task has nothing left to cancel.
fn plan_cancel(state: &TaskState) -> CancelPlan {
    match state {
        TaskState::Queued | TaskState::Paused { .. } | TaskState::Failed { .. } => {
            CancelPlan::Directly
        }
        TaskState::Preflight
        | TaskState::Running { .. }
        | TaskState::Remediating { .. }
        | TaskState::Verifying { .. } => CancelPlan::ThroughSupervisor,
        TaskState::Publishing { .. } => CancelPlan::Refuse(
            "is publishing; a half-published change cannot be cancelled, and once it is \
             published it is done"
                .to_string(),
        ),
        TaskState::PublishedVerified { .. } | TaskState::Done | TaskState::Acknowledged { .. } => {
            CancelPlan::Refuse(format!(
                "is already {}; there is nothing to cancel",
                state.name().to_lowercase()
            ))
        }
        TaskState::Cancelled => CancelPlan::Refuse("is already cancelled".to_string()),
    }
}

/// Whether a supervisor is in the middle of `state`: an attempt is in
/// flight, and it is the supervisor's to finish or stop, not the journal's.
/// `PublishedVerified` is not: the change is already on mainline, and only
/// the bookkeeping that follows is left.
pub(super) fn in_flight(state: &TaskState) -> bool {
    matches!(
        state,
        TaskState::Preflight
            | TaskState::Running { .. }
            | TaskState::Remediating { .. }
            | TaskState::Verifying { .. }
            | TaskState::Publishing { .. }
    )
}

/// The task a supervisor is running, in queue order.
fn find_in_flight(tasks: &[Task], states: &BTreeMap<TaskId, TaskState>) -> Option<TaskId> {
    tasks
        .iter()
        .map(|task| task.id)
        .find(|id| states.get(id).is_some_and(in_flight))
}

/// The task `verb` (`pause` or `interrupt`) applies to: the one being run,
/// if its supervisor is still there to hear it.
///
/// # Errors
///
/// Returns the outcome to exit with: usage (2) when no task is running or
/// the record of one is orphaned, and a failed check when the journal
/// cannot be read.
fn running_task(verb: &str, project: &Project) -> Result<TaskId, RunOutcome> {
    let (tasks, states) = read_queue(project)
        .map_err(|err| check_failed(format!("{verb}: could not read the queue: {err}")))?;
    let Some(running) = find_in_flight(&tasks, &states) else {
        return Err(RunOutcome::Usage {
            detail: format!("{verb}: no task is running; there is nothing to {verb}"),
        });
    };
    match supervisor_alive(project, running) {
        Ok(true) => Ok(running),
        Ok(false) => Err(orphaned(
            verb,
            running,
            states.get(&running).unwrap_or(&TaskState::Queued),
        )),
        Err(err) => Err(check_failed(format!("{verb}: {err}"))),
    }
}

/// The usage error for a task the journal says is running whose supervisor
/// has exited: there is no one to ask, and only `resume` — which reconciles
/// the journal with reality first — can decide what became of it.
fn orphaned(verb: &str, task: TaskId, state: &TaskState) -> RunOutcome {
    RunOutcome::Usage {
        detail: format!(
            "{verb}: task {task} is {} but no supervisor is running it; \
             `ktask-rs resume` reconciles it",
            state.name().to_lowercase()
        ),
    }
}

/// Waits until `task` is no longer in flight and returns the state it
/// settled in.
///
/// # Errors
///
/// Returns the outcome to exit with: a failed check when the journal cannot
/// be read, or when `task` is still in flight after `timeout` (the request
/// stays pending, so the supervisor will still act on it at its next
/// boundary).
fn settle(
    verb: &str,
    project: &Project,
    task: TaskId,
    timeout: Duration,
) -> Result<TaskState, RunOutcome> {
    let deadline = Instant::now() + timeout;
    loop {
        let (_, states) = read_queue(project)
            .map_err(|err| check_failed(format!("{verb}: could not read the queue: {err}")))?;
        let state = states.get(&task).cloned().unwrap_or(TaskState::Queued);
        if !in_flight(&state) {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(check_failed(format!(
                "{verb}: task {task} is still {} after {timeout:?}; the request stays pending \
                 and takes effect at the supervisor's next boundary",
                state.name().to_lowercase()
            )));
        }
        thread::sleep(SETTLE_POLL);
    }
}

/// [`RunOutcome::CheckFailed`] for `detail`, also printed to stderr.
fn check_failed(detail: String) -> RunOutcome {
    render::progress(format_args!("error: {detail}"));
    RunOutcome::CheckFailed { detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{
        AttemptId, FailureClass, PauseReason, Phase, TaskStatus, next_runnable, pending,
    };
    use std::process;

    fn task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "true".to_string(),
            refs: "none".to_string(),
            protocol: None,
        }
    }

    /// The events that leave a task running an attempt, its supervisor
    /// being process `pid`.
    fn running(pid: u32) -> Vec<EventKind> {
        vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid,
                base_sha: "abc".to_string(),
            },
        ]
    }

    /// [`running`], carried on to the moment of publication.
    fn publishing(pid: u32) -> Vec<EventKind> {
        let mut events = running(pid);
        events.push(EventKind::PhaseEntered {
            attempt: AttemptId::new(1),
            phase: Phase::Verify,
        });
        events.push(EventKind::PublishStarted {
            attempt: AttemptId::new(1),
            candidate_sha: "def".to_string(),
        });
        events
    }

    fn failed(pid: u32) -> Vec<EventKind> {
        let mut events = running(pid);
        events.push(EventKind::TaskFailed {
            class: FailureClass::AgentFailure,
            detail: "boom".to_string(),
        });
        events
    }

    fn done(pid: u32) -> Vec<EventKind> {
        let mut events = publishing(pid);
        events.push(EventKind::PublishVerified {
            commit: "def".to_string(),
            remote_sha: "def".to_string(),
        });
        events.push(EventKind::TaskDone {
            commit: "def".to_string(),
        });
        events
    }

    fn input_pause() -> Vec<EventKind> {
        vec![EventKind::Paused {
            reason: PauseReason::Input,
        }]
    }

    /// A project whose journal holds one task per entry, each carrying the
    /// events listed.
    fn project_with(dir: &tempfile::TempDir, tasks: &[Vec<EventKind>]) -> Project {
        let project = Project {
            root: dir.path().join("repo"),
            id: "control-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        let mut journal = Journal::open_for(&project).expect("open journal");
        let all: Vec<Task> = (1..=u32::try_from(tasks.len()).expect("few tasks"))
            .map(task)
            .collect();
        journal.put_tasks(&all).expect("put tasks");
        for (index, events) in tasks.iter().enumerate() {
            let id = TaskId::new(u32::try_from(index + 1).expect("few tasks"));
            for event in events {
                journal.append(Some(id), event).expect("append");
            }
        }
        project
    }

    fn state_of(project: &Project, id: u32) -> TaskState {
        let (_, states) = read_queue(project).expect("read queue");
        states[&TaskId::new(id)].clone()
    }

    fn kinds_of(project: &Project, id: u32) -> Vec<&'static str> {
        Journal::open_for(project)
            .expect("journal")
            .events_for(TaskId::new(id))
            .expect("events")
            .iter()
            .map(|event| event.kind.discriminant())
            .collect()
    }

    /// The supervisor, as a thread: waits for `request` to be sent, then
    /// journals `event` against `task`, as the real one does on finding it.
    fn supervisor_acting(
        project: &Project,
        request: Request,
        task: u32,
        event: EventKind,
    ) -> thread::JoinHandle<()> {
        let project = project.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !pending(&project, request) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(pending(&project, request), "the request was never sent");
            Journal::open_for(&project)
                .expect("journal")
                .append(Some(TaskId::new(task)), &event)
                .expect("append");
        })
    }

    /// A process id that is not running.
    fn dead_pid() -> u32 {
        let mut child = process::Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait");
        pid
    }

    const SHORT: Duration = Duration::from_millis(200);

    // -- the pure rules -------------------------------------------------------

    fn every_state() -> Vec<TaskState> {
        let running = TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        };
        vec![
            TaskState::Queued,
            TaskState::Preflight,
            running.clone(),
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
            TaskState::PublishedVerified {
                commit: "def".to_string(),
            },
            TaskState::Done,
            TaskState::Acknowledged {
                by: "me".to_string(),
                at: time::OffsetDateTime::UNIX_EPOCH,
            },
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(running),
            },
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "boom".to_string(),
            },
            TaskState::Cancelled,
        ]
    }

    #[test]
    fn only_a_task_with_an_attempt_in_flight_counts_as_running() {
        let running: Vec<&str> = every_state()
            .iter()
            .filter(|state| in_flight(state))
            .map(TaskState::name)
            .collect();

        assert_eq!(
            running,
            vec![
                "Preflight",
                "Running",
                "Remediating",
                "Verifying",
                "Publishing"
            ]
        );
    }

    #[test]
    fn cancel_acts_directly_on_a_task_nothing_is_running() {
        for state in every_state() {
            let expected = matches!(
                state,
                TaskState::Queued | TaskState::Paused { .. } | TaskState::Failed { .. }
            );
            assert_eq!(
                plan_cancel(&state) == CancelPlan::Directly,
                expected,
                "{}",
                state.name()
            );
        }
    }

    #[test]
    fn cancel_asks_the_supervisor_only_for_a_task_it_is_running_and_can_stop() {
        for state in every_state() {
            let expected = matches!(
                state,
                TaskState::Preflight
                    | TaskState::Running { .. }
                    | TaskState::Remediating { .. }
                    | TaskState::Verifying { .. }
            );
            assert_eq!(
                plan_cancel(&state) == CancelPlan::ThroughSupervisor,
                expected,
                "{}",
                state.name()
            );
        }
    }

    #[test]
    fn cancel_refuses_a_publishing_or_finished_task_saying_why() {
        let text = |state: &TaskState| match plan_cancel(state) {
            CancelPlan::Refuse(text) => text,
            other => panic!("{} was not refused: {other:?}", state.name()),
        };

        assert!(
            text(&TaskState::Publishing {
                attempt: AttemptId::new(1)
            })
            .contains("publishing")
        );
        assert!(text(&TaskState::Done).contains("already done"));
        assert!(text(&TaskState::Cancelled).contains("already cancelled"));
        assert!(
            text(&TaskState::PublishedVerified {
                commit: "x".to_string()
            })
            .contains("already publishedverified")
        );
    }

    #[test]
    fn the_running_task_is_found_in_queue_order_or_not_at_all() {
        let tasks = [task(1), task(2), task(3)];
        let mut states = BTreeMap::new();
        states.insert(TaskId::new(1), TaskState::Done);
        states.insert(TaskId::new(2), TaskState::Preflight);
        states.insert(TaskId::new(3), TaskState::Queued);

        assert_eq!(find_in_flight(&tasks, &states), Some(TaskId::new(2)));

        states.insert(TaskId::new(2), TaskState::Done);
        assert_eq!(find_in_flight(&tasks, &states), None);
    }

    // -- pause ----------------------------------------------------------------

    #[test]
    fn pause_exits_2_when_no_task_is_running_and_sends_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[done(process::id()), vec![]]);

        let outcome = pause(&project, &Config::default());

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("nothing to pause")),
            "{outcome:?}"
        );
        assert!(!pending(&project, Request::Pause));
    }

    #[test]
    fn pause_asks_a_running_supervisor_and_journals_nothing_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id()), vec![]]);
        let before = kinds_of(&project, 1);

        let outcome = pause(&project, &Config::default());

        assert_eq!(outcome, RunOutcome::Drained);
        assert!(pending(&project, Request::Pause));
        assert!(!pending(&project, Request::Interrupt));
        assert_eq!(kinds_of(&project, 1), before, "the supervisor journals it");
        assert!(matches!(state_of(&project, 1), TaskState::Running { .. }));
    }

    #[test]
    fn pause_exits_2_when_the_running_tasks_supervisor_has_exited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(dead_pid())]);

        let outcome = pause(&project, &Config::default());

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail }
                if detail.contains("no supervisor") && detail.contains("resume")),
            "{outcome:?}"
        );
        assert!(!pending(&project, Request::Pause));
    }

    #[test]
    fn pause_fails_the_check_when_the_journal_cannot_be_read() {
        let project = Project {
            root: "/nonexistent/ktask-control/root".into(),
            id: "control-fixture".to_string(),
            state_dir: "/nonexistent/ktask-control/state".into(),
        };

        assert!(matches!(
            pause(&project, &Config::default()),
            RunOutcome::CheckFailed { .. }
        ));
    }

    // -- interrupt ------------------------------------------------------------

    #[test]
    fn interrupt_exits_2_when_no_task_is_running_and_sends_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[vec![]]);

        let outcome = interrupt_within(&project, SHORT);

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("nothing to interrupt")),
            "{outcome:?}"
        );
        assert!(!pending(&project, Request::Interrupt));
    }

    #[test]
    fn interrupt_succeeds_once_the_supervisor_has_journaled_the_interruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id())]);
        let supervisor = supervisor_acting(
            &project,
            Request::Interrupt,
            1,
            EventKind::Interrupted { phase: Phase::Goal },
        );

        let outcome = interrupt_within(&project, Duration::from_secs(10));
        supervisor.join().expect("supervisor");

        assert_eq!(outcome, RunOutcome::Drained);
        assert!(matches!(
            state_of(&project, 1),
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn interrupt_reports_a_supervisor_that_has_not_acted_and_leaves_the_request_pending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id())]);

        let outcome = interrupt_within(&project, SHORT);

        assert!(
            matches!(&outcome, RunOutcome::CheckFailed { detail }
                if detail.contains("still running") && detail.contains("pending")),
            "{outcome:?}"
        );
        assert!(pending(&project, Request::Interrupt));
    }

    #[test]
    fn interrupt_exits_2_when_the_task_stopped_some_other_way_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id())]);
        let supervisor = supervisor_acting(
            &project,
            Request::Interrupt,
            1,
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "boom".to_string(),
            },
        );

        let outcome = interrupt_within(&project, Duration::from_secs(10));
        supervisor.join().expect("supervisor");

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("failed")),
            "{outcome:?}"
        );
    }

    // -- cancel ---------------------------------------------------------------

    #[test]
    fn cancel_of_a_task_not_in_the_queue_exits_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[vec![]]);

        let outcome = cancel_within(&project, TaskId::new(9), SHORT);

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "no task 9 in the queue".to_string()
            }
        );
    }

    #[test]
    fn cancelling_a_failed_task_makes_its_successor_runnable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[failed(process::id()), vec![]]);
        let (tasks, states) = read_queue(&project).expect("queue");
        assert_eq!(
            next_runnable(&tasks, &states).expect("next"),
            None,
            "a failed task blocks the queue"
        );

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, 1), TaskState::Cancelled);
        let (tasks, states) = read_queue(&project).expect("queue");
        assert_eq!(
            next_runnable(&tasks, &states).expect("next"),
            Some(TaskId::new(2))
        );
    }

    #[test]
    fn cancelling_a_queued_task_journals_a_task_cancelled_naming_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[vec![], vec![]]);

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert_eq!(outcome, RunOutcome::Drained);
        let events = Journal::open_for(&project)
            .expect("journal")
            .events_for(TaskId::new(1))
            .expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind,
            EventKind::TaskCancelled {
                reason: CANCEL_REASON.to_string()
            }
        );
        assert_eq!(state_of(&project, 2), TaskState::Queued, "only task 1");
    }

    #[test]
    fn cancelling_a_task_paused_for_input_cancels_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[input_pause(), vec![]]);

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, 1), TaskState::Cancelled);
    }

    #[test]
    fn cancelling_a_finished_task_exits_2_and_journals_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[done(process::id())]);
        let before = kinds_of(&project, 1);

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("already done")),
            "{outcome:?}"
        );
        assert_eq!(kinds_of(&project, 1), before);
    }

    #[test]
    fn cancelling_a_task_twice_exits_2_the_second_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[vec![]]);
        assert_eq!(
            cancel_within(&project, TaskId::new(1), SHORT),
            RunOutcome::Drained
        );

        let again = cancel_within(&project, TaskId::new(1), SHORT);

        assert!(matches!(again, RunOutcome::Usage { .. }), "{again:?}");
        assert_eq!(kinds_of(&project, 1), vec!["TaskCancelled"]);
    }

    #[test]
    fn cancelling_a_publishing_task_exits_2_and_sends_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[publishing(process::id())]);

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("publishing")),
            "{outcome:?}"
        );
        assert!(!pending(&project, Request::Cancel(TaskId::new(1))));
    }

    #[test]
    fn cancelling_a_running_task_asks_its_supervisor_rather_than_journaling_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id()), vec![]]);
        let supervisor = supervisor_acting(
            &project,
            Request::Cancel(TaskId::new(1)),
            1,
            EventKind::TaskCancelled {
                reason: "by the supervisor".to_string(),
            },
        );

        let outcome = cancel_within(&project, TaskId::new(1), Duration::from_secs(10));
        supervisor.join().expect("supervisor");

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, 1), TaskState::Cancelled);
        assert_eq!(
            kinds_of(&project, 1)
                .iter()
                .filter(|kind| **kind == "TaskCancelled")
                .count(),
            1,
            "exactly the supervisor's own event"
        );
    }

    #[test]
    fn cancelling_a_running_task_whose_supervisor_has_exited_exits_2_and_journals_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(dead_pid())]);
        let before = kinds_of(&project, 1);

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail }
                if detail.contains("no supervisor") && detail.contains("resume")),
            "{outcome:?}"
        );
        assert_eq!(kinds_of(&project, 1), before);
        assert!(!pending(&project, Request::Cancel(TaskId::new(1))));
    }

    #[test]
    fn cancelling_a_running_task_that_finishes_first_exits_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id())]);
        let supervisor = supervisor_acting(
            &project,
            Request::Cancel(TaskId::new(1)),
            1,
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "boom".to_string(),
            },
        );

        let outcome = cancel_within(&project, TaskId::new(1), Duration::from_secs(10));
        supervisor.join().expect("supervisor");

        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("failed")),
            "{outcome:?}"
        );
    }

    #[test]
    fn cancelling_a_running_task_reports_a_supervisor_that_has_not_acted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_with(&dir, &[running(process::id())]);

        let outcome = cancel_within(&project, TaskId::new(1), SHORT);

        assert!(
            matches!(&outcome, RunOutcome::CheckFailed { detail } if detail.contains("still running")),
            "{outcome:?}"
        );
        assert!(pending(&project, Request::Cancel(TaskId::new(1))));
    }
}

//! `ktask-rs run`: drains the queue in order, strictly serially
//! (`docs/CONTRACT.md` section 3).
//!
//! [`run`] builds a [`Runner`] — which constructs the provider the project's
//! configuration names — subscribes to its bus, and hands the queue to
//! [`Runner::run_queue`]. While that blocks, a second thread drains the
//! subscription: every event becomes a progress line on stderr
//! ([`progress_line`]), and the events that settle or park a task also become
//! that task's result line on stdout ([`result_of`]). The [`RunOutcome`]
//! [`Runner::run_queue`] returns is what the process exits with; nothing is
//! inferred from the text printed.
//!
//! Everything below [`run`] that decides anything — which tasks are in scope,
//! whether a queue that "drained" really did, how an event reads — is a pure
//! function over already-read values, so it is tested without a journal, a
//! provider or a clock.

use ktask_core::{
    Config, Event, EventKind, Journal, PauseReason, Project, RecoveryDecision, RunOutcome, Runner,
    Task, TaskId, TaskState, apply, check_predecessor, reconcile,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::{json, render};

/// How often the progress thread checks the bus for new events. Short
/// enough that output feels live; the bus's ring is what absorbs bursts in
/// between.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The longest a result line's detail may run on stdout, so a task's result
/// stays one readable line however long the failure text behind it is.
const DETAIL_LIMIT: usize = 200;

/// One task's result line, in human and `--json` form alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct TaskResult {
    pub(super) task: TaskId,
    /// `done`, `failed`, `paused` or `interrupted`.
    pub(super) result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
}

impl TaskResult {
    /// The single stdout line for this result: `task 3: done (commit abc)`.
    fn line(&self) -> String {
        match &self.detail {
            Some(detail) => format!("task {}: {} ({})", self.task, self.result, one_line(detail)),
            None => format!("task {}: {}", self.task, self.result),
        }
    }
}

/// What the progress thread saw, handed back once the run ends.
#[derive(Debug, Default)]
pub(super) struct Pumped {
    /// Tasks whose result line has already been printed.
    pub(super) reported: BTreeSet<TaskId>,
    /// Events the bus evicted before the thread read them.
    pub(super) dropped: usize,
}

/// Runs the queue (or, with `task`, exactly one task; with `from`, from that
/// id on) against the project's configured provider.
///
/// Before anything is selected, the journal is reconciled with reality
/// ([`recover`]): a run that was killed mid-attempt is resolved to a known
/// state, and each decision is printed, so the queue is never read in a state
/// the last run left half-finished (`VISION.md` §6).
///
/// A queue that cannot be started is [`RunOutcome::Usage`] (an unbuildable
/// provider or config, an id that is not in the queue, a `--task` that is
/// not next in line) or [`RunOutcome::CheckFailed`] (the journal cannot be
/// read). Otherwise the outcome is [`Runner::run_queue`]'s, corrected in one
/// case: the runner reports [`RunOutcome::Drained`] for a queue that is
/// merely blocked — a task already failed or paused from an earlier run — and
/// reporting that as success would let a script carry on past it, so
/// [`blocked_outcome`] turns it back into the pause or failure it is.
pub(crate) fn run(
    project: &Project,
    config: &Config,
    task: Option<TaskId>,
    from: Option<TaskId>,
    json_output: bool,
) -> RunOutcome {
    summarize(drain(project, Some(config), task, from, json_output))
}

/// Prints `outcome`'s closing stderr line and hands it back.
fn summarize(outcome: RunOutcome) -> RunOutcome {
    match &outcome {
        RunOutcome::Usage { .. } => {}
        RunOutcome::CheckFailed { detail } => render::progress(format_args!("error: {detail}")),
        other => render::progress(format_args!("run: {}", describe(other))),
    }
    outcome
}

/// [`run`] for a caller that has already reconciled the journal itself
/// ([`recover`]) and must not have it done a second time: a task left
/// paused on an interruption is decided afresh on every reconciliation.
pub(super) fn run_reconciled(
    project: &Project,
    task: Option<TaskId>,
    from: Option<TaskId>,
    json_output: bool,
) -> RunOutcome {
    summarize(drain(project, None, task, from, json_output))
}

/// Reconciles `project`'s journal with reality after a restart and prints
/// each decision to stderr, one line apiece.
///
/// The decisions themselves are journaled by [`reconcile`]; this only tells
/// the person at the terminal what was found. A clean journal reconciles to
/// no decisions and prints nothing.
///
/// # Errors
///
/// Returns whatever [`Journal::open_for`] or [`reconcile`] return.
pub(super) fn recover(project: &Project, config: &Config) -> ktask_core::Result<()> {
    let mut journal = Journal::open_for(project)?;
    for decision in reconcile(&mut journal, project, config)? {
        render::progress(format_args!("{}", recovery_line(&decision)));
    }
    Ok(())
}

/// The stderr line for one recovery `decision`.
fn recovery_line(decision: &RecoveryDecision) -> String {
    match decision {
        RecoveryDecision::StateRebuilt => {
            "recovery: rebuilt the task state from the journal; it had fallen behind".to_string()
        }
        RecoveryDecision::Task {
            task,
            decision,
            detail,
        } => format!("recovery: task {task}: {decision:?}: {}", one_line(detail)),
    }
}

/// [`run`] without its closing summary line. `reconcile_with` is the config
/// to reconcile the journal against before selecting a task, or `None` when
/// the caller already has.
fn drain(
    project: &Project,
    reconcile_with: Option<&Config>,
    task: Option<TaskId>,
    from: Option<TaskId>,
    json_output: bool,
) -> RunOutcome {
    let mut runner = match Runner::new(project.clone()) {
        Ok(runner) => runner,
        Err(err) => {
            return RunOutcome::Usage {
                detail: format!("run: {err}"),
            };
        }
    };
    if let Some(config) = reconcile_with
        && let Err(err) = recover(project, config)
    {
        return RunOutcome::CheckFailed {
            detail: format!("run: recovery failed: {err}"),
        };
    }
    let (tasks, states) = match read_queue(project) {
        Ok(queue) => queue,
        Err(err) => {
            return RunOutcome::CheckFailed {
                detail: format!("run: could not read the queue: {err}"),
            };
        }
    };
    let (scope, from) = match select(&tasks, &states, task, from) {
        Ok(selected) => selected,
        Err(detail) => return RunOutcome::Usage { detail },
    };

    let (result, pumped) = run_with_progress(&mut runner, json_output, |runner| {
        runner.run_queue(&scope, from)
    });
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            return RunOutcome::CheckFailed {
                detail: format!("run: {err}"),
            };
        }
    };
    if pumped.dropped > 0 {
        render::progress(format_args!(
            "run: {} progress events were not shown; `ktask-rs status` has the final state \
             of every task",
            pumped.dropped
        ));
    }

    match outcome {
        RunOutcome::Drained => match read_queue(project) {
            Ok((_, states)) => blocked_outcome(&scope, from, &states).unwrap_or(outcome),
            Err(err) => RunOutcome::CheckFailed {
                detail: format!("run: could not re-read the queue: {err}"),
            },
        },
        RunOutcome::TaskFailed { task } => {
            // The runner reports a failure it could not journal as an event
            // (a provider that errored outright, say) only through its
            // return value: give that task its result line all the same.
            if !pumped.reported.contains(&task) {
                emit_result(
                    &TaskResult {
                        task,
                        result: "failed",
                        detail: None,
                    },
                    json_output,
                );
            }
            outcome
        }
        other => other,
    }
}

/// Runs `work` against `runner`, printing progress and result lines from a
/// second thread for as long as it takes.
///
/// `runner` stays on the calling thread; only the [`ktask_core::Subscription`]
/// crosses to the printer, which keeps draining until `work` has returned
/// and then once more, so nothing recorded before it ended is lost.
pub(super) fn run_with_progress<T>(
    runner: &mut Runner,
    json_output: bool,
    work: impl FnOnce(&mut Runner) -> ktask_core::Result<T>,
) -> (ktask_core::Result<T>, Pumped) {
    let mut subscription = runner.subscribe();
    let finished = AtomicBool::new(false);
    let finished = &finished;

    thread::scope(|threads| {
        let printer = threads.spawn(move || {
            let mut pumped = Pumped::default();
            loop {
                let last = finished.load(Ordering::SeqCst);
                let (events, dropped) = subscription.drain();
                pumped.dropped += dropped;
                for event in &events {
                    print_event(event, json_output, &mut pumped.reported);
                }
                if last {
                    return pumped;
                }
                thread::sleep(POLL_INTERVAL);
            }
        });

        let result = work(runner);
        finished.store(true, Ordering::SeqCst);
        (result, printer.join().unwrap_or_default())
    })
}

/// Prints `event`'s progress line to stderr and, when it settles or parks
/// its task, that task's result line to stdout — noting the task in
/// `reported` so [`drain`] does not print it a second time.
fn print_event(event: &Event, json_output: bool, reported: &mut BTreeSet<TaskId>) {
    if let Some(line) = progress_line(event) {
        render::progress(format_args!("{line}"));
    }
    if let Some(result) = result_of(event) {
        emit_result(&result, json_output);
        reported.insert(result.task);
    }
}

/// Writes `result` to stdout: one JSON object under `--json`, otherwise its
/// human line.
pub(super) fn emit_result(result: &TaskResult, json_output: bool) {
    if json_output {
        let _ = json::emit_json(result);
    } else {
        render::out(format_args!("{}", result.line()));
    }
}

/// Reads the queue and each task's current state, in queue order.
///
/// A state is the fold of the task's journaled events ([`apply`]), exactly
/// as [`Runner::run_queue`] reads it, not the `task_state` projection: that
/// is only rebuilt by recovery, so between runs it can lag the journal that
/// is the source of truth. A task with no events has not started, so it is
/// [`TaskState::Queued`].
pub(super) fn read_queue(
    project: &Project,
) -> ktask_core::Result<(Vec<Task>, BTreeMap<TaskId, TaskState>)> {
    let journal = Journal::open_for(project)?;
    let tasks = journal.tasks()?;
    let mut states = BTreeMap::new();
    for task in &tasks {
        let state = journal
            .events_for(task.id)?
            .into_iter()
            .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))?;
        states.insert(task.id, state);
    }
    Ok((tasks, states))
}

/// Narrows `tasks` to what `--task` and `--from` ask for, returning the
/// tasks to hand [`Runner::run_queue`] and the `from` to hand it with them.
///
/// `--from` names an id that must exist. `--task` runs exactly one task, so
/// it hands `run_queue` that task alone — but only if it is [queued] and
/// every task ahead of it has already been published: the queue is strictly
/// ordered, and a single-task run may not be the way around that.
///
/// [queued]: TaskState::Queued
///
/// # Errors
///
/// Returns the usage-error text for an id not in the queue, a `--task` that
/// is not queued, or a `--task` whose predecessor is not yet published.
fn select(
    tasks: &[Task],
    states: &BTreeMap<TaskId, TaskState>,
    task: Option<TaskId>,
    from: Option<TaskId>,
) -> Result<(Vec<Task>, Option<TaskId>), String> {
    let find = |id: TaskId| {
        tasks
            .iter()
            .find(|candidate| candidate.id == id)
            .ok_or_else(|| format!("no task {id} in the queue"))
    };

    if let Some(id) = task {
        let chosen = find(id)?;
        let state = states.get(&id).unwrap_or(&TaskState::Queued);
        if *state != TaskState::Queued {
            return Err(format!(
                "task {id} is {}, not queued; only a queued task can be run",
                state.name()
            ));
        }
        check_predecessor(states, id).map_err(|err| err.to_string())?;
        return Ok((vec![chosen.clone()], None));
    }

    if let Some(id) = from {
        find(id)?;
    }
    Ok((tasks.to_vec(), from))
}

/// The pause or failure hiding behind a [`RunOutcome::Drained`] the runner
/// returned for `scope`, if there is one.
///
/// [`Runner::run_queue`] selects nothing — and so reports the queue drained —
/// whenever any task is failed, paused or still in flight, including from an
/// earlier run. The first such task in `scope` (from `from` on) is the
/// reason nothing ran. A task left mid-attempt (its run was killed, or its
/// attempt errored before the runner could journal a verdict) is neither done
/// nor failed on the record: it is reported as [`RunOutcome::Interrupted`],
/// the resumable pause, rather than passed over as if it were finished.
fn blocked_outcome(
    scope: &[Task],
    from: Option<TaskId>,
    states: &BTreeMap<TaskId, TaskState>,
) -> Option<RunOutcome> {
    scope
        .iter()
        .filter(|task| from.is_none_or(|from| task.id >= from))
        .find_map(|task| match states.get(&task.id)? {
            TaskState::Failed { .. } => Some(RunOutcome::TaskFailed { task: task.id }),
            TaskState::Paused { reason, .. } => Some(match reason {
                PauseReason::HumanGate => RunOutcome::HumanGate { task: task.id },
                PauseReason::Input => RunOutcome::NeedsInput { task: task.id },
                PauseReason::Limit { until } => RunOutcome::ProviderLimit { until: *until },
                PauseReason::Interrupted | PauseReason::Blocked => RunOutcome::Interrupted,
            }),
            TaskState::Preflight
            | TaskState::Running { .. }
            | TaskState::Remediating { .. }
            | TaskState::Verifying { .. }
            | TaskState::Publishing { .. } => Some(RunOutcome::Interrupted),
            TaskState::Queued
            | TaskState::PublishedVerified { .. }
            | TaskState::Done
            | TaskState::Acknowledged { .. }
            | TaskState::Cancelled => None,
        })
}

/// The result line `event` produces, if it is one that settles or parks a
/// task.
fn result_of(event: &Event) -> Option<TaskResult> {
    let task = event.task_id?;
    let (result, detail) = match &event.kind {
        EventKind::TaskDone { commit } => ("done", Some(format!("commit {}", short(commit)))),
        EventKind::TaskFailed { class, detail } => ("failed", Some(format!("{class:?}: {detail}"))),
        EventKind::Paused { reason } => ("paused", Some(pause_text(reason))),
        EventKind::Interrupted { phase } => ("interrupted", Some(format!("during {phase:?}"))),
        _ => return None,
    };
    Some(TaskResult {
        task,
        result,
        detail,
    })
}

/// The stderr progress text for `event`, or `None` for events too internal
/// to show. Multi-line agent output becomes one line per line of output.
fn progress_line(event: &Event) -> Option<String> {
    let who = event
        .task_id
        .map_or_else(|| "run".to_string(), |id| format!("task {id}"));
    let text = match &event.kind {
        EventKind::PreflightStarted => "preflight: checking the repository".to_string(),
        EventKind::PreflightPassed { base_sha } => {
            format!("preflight: passed (base {})", short(base_sha))
        }
        EventKind::PreflightFailed { class, detail } => {
            format!("preflight: failed ({class:?}): {detail}")
        }
        EventKind::AttemptStarted {
            attempt, protocol, ..
        } => format!("attempt {attempt} started ({protocol})"),
        EventKind::PhaseEntered { attempt, phase } => {
            format!("attempt {attempt}: phase {phase:?}")
        }
        EventKind::AgentOutput { text, .. } => {
            return (!text.trim().is_empty()).then(|| {
                text.lines()
                    .map(|line| format!("{who} | {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        EventKind::AttemptFinished {
            attempt, exit_code, ..
        } => format!("attempt {attempt} finished (exit {exit_code})"),
        EventKind::GateStarted { gate } => format!("gate {gate:?}: running"),
        EventKind::GateFinished { result } => format!(
            "gate {:?}: {}",
            result.kind,
            if result.passed { "passed" } else { "failed" }
        ),
        EventKind::VerifyPassed { attempt } => format!("attempt {attempt}: verified"),
        EventKind::VerifyFailed {
            attempt,
            class,
            detail,
        } => format!("attempt {attempt}: verification failed ({class:?}): {detail}"),
        EventKind::PublishStarted { candidate_sha, .. } => {
            format!("publishing {}", short(candidate_sha))
        }
        EventKind::PublishVerified { commit, .. } => {
            format!("published {} (confirmed on the remote)", short(commit))
        }
        EventKind::TaskDone { .. } => "done".to_string(),
        EventKind::TaskFailed { class, detail } => format!("failed ({class:?}): {detail}"),
        EventKind::TaskCancelled { reason } => format!("cancelled: {reason}"),
        EventKind::Paused { reason } => format!("paused: {}", pause_text(reason)),
        EventKind::Resumed => "resumed".to_string(),
        EventKind::RetryStarted { attempt } => {
            format!("retry: attempt {attempt} started in a fresh session")
        }
        EventKind::Interrupted { phase } => format!("interrupted during {phase:?}"),
        EventKind::RecoveryDecision { decision, detail } => {
            format!("recovery: {decision:?}: {detail}")
        }
        EventKind::TddExceptionUsed { exception, reason } => {
            format!("tdd exception {exception:?}: {reason}")
        }
        EventKind::DecisionRaised { request } => format!("needs input: {}", request.question),
        EventKind::DecisionResolved { adr_path, .. } => {
            format!("decision resolved: {}", adr_path.display())
        }
        EventKind::SelfHealingReport {
            attempt,
            class,
            outcome,
            ..
        } => format!("attempt {attempt}: remediation after {class:?}: {outcome}"),
        EventKind::TaskQueued { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. } => return None,
    };
    Some(format!("{who}: {text}"))
}

/// Why a task is paused, in words a person can act on.
fn pause_text(reason: &PauseReason) -> String {
    match reason {
        PauseReason::HumanGate => "human gate; `ktask-rs ack` passes it".to_string(),
        PauseReason::Input => "needs input; `ktask-rs resolve` answers it".to_string(),
        PauseReason::Limit { until: Some(until) } => {
            format!("provider limit reached until {until}")
        }
        PauseReason::Limit { until: None } => "provider limit reached".to_string(),
        PauseReason::Interrupted => "interrupted".to_string(),
        PauseReason::Blocked => "blocked".to_string(),
    }
}

/// The closing stderr sentence for `outcome`.
pub(super) fn describe(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Drained => "queue drained".to_string(),
        RunOutcome::TaskFailed { task } => {
            format!("stopped: task {task} failed; later tasks were not started")
        }
        RunOutcome::ProviderLimit { until: Some(until) } => {
            format!("stopped: provider limit reached until {until}; `ktask-rs resume` continues")
        }
        RunOutcome::ProviderLimit { until: None } => {
            "stopped: provider limit reached; `ktask-rs resume` continues".to_string()
        }
        RunOutcome::HumanGate { task } => {
            format!("stopped: task {task} is at a human gate; `ktask-rs ack` passes it")
        }
        RunOutcome::NeedsInput { task } => {
            format!("stopped: task {task} needs input; `ktask-rs resolve --task {task}` answers it")
        }
        RunOutcome::Interrupted => "interrupted; `ktask-rs resume` continues".to_string(),
        RunOutcome::CheckFailed { detail } | RunOutcome::Usage { detail } => detail.clone(),
    }
}

/// The first seven characters of a commit id.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// `text` on one line — newlines folded to spaces — and no longer than
/// [`DETAIL_LIMIT`] characters.
fn one_line(text: &str) -> String {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.chars().count() <= DETAIL_LIMIT {
        return folded;
    }
    let mut cut: String = folded.chars().take(DETAIL_LIMIT).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{AttemptId, EventSeq, FailureClass, Phase, Stream, TaskStatus};
    use time::OffsetDateTime;

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

    fn tasks(count: u32) -> Vec<Task> {
        (1..=count).map(task).collect()
    }

    fn states(list: &[(u32, TaskState)]) -> BTreeMap<TaskId, TaskState> {
        list.iter()
            .map(|(id, state)| (TaskId::new(*id), state.clone()))
            .collect()
    }

    fn event(task: Option<u32>, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: task.map(TaskId::new),
            kind,
        }
    }

    fn paused(reason: PauseReason) -> TaskState {
        TaskState::Paused {
            reason,
            resume_to: Box::new(TaskState::Queued),
        }
    }

    fn failed() -> TaskState {
        TaskState::Failed {
            class: FailureClass::AgentFailure,
            detail: "boom".to_string(),
        }
    }

    // ---- select ----

    #[test]
    fn select_with_no_flags_hands_over_the_whole_queue() {
        let queue = tasks(3);
        let (scope, from) = select(&queue, &BTreeMap::new(), None, None).expect("select");
        assert_eq!(scope, queue);
        assert_eq!(from, None);
    }

    #[test]
    fn select_from_keeps_the_whole_queue_and_passes_the_id_on() {
        let queue = tasks(3);
        let (scope, from) =
            select(&queue, &BTreeMap::new(), None, Some(TaskId::new(2))).expect("select");
        assert_eq!(scope, queue, "run_queue applies `from` itself");
        assert_eq!(from, Some(TaskId::new(2)));
    }

    #[test]
    fn select_task_hands_over_exactly_that_task() {
        let queue = tasks(3);
        let (scope, from) = select(
            &queue,
            &states(&[(1, TaskState::Done)]),
            Some(TaskId::new(2)),
            None,
        )
        .expect("select");
        assert_eq!(scope, vec![task(2)]);
        assert_eq!(from, None);
    }

    #[test]
    fn select_rejects_ids_that_are_not_in_the_queue() {
        let queue = tasks(2);
        let by_task = select(&queue, &BTreeMap::new(), Some(TaskId::new(7)), None);
        let by_from = select(&queue, &BTreeMap::new(), None, Some(TaskId::new(7)));
        assert_eq!(by_task, Err("no task 7 in the queue".to_string()));
        assert_eq!(by_from, Err("no task 7 in the queue".to_string()));
    }

    #[test]
    fn select_task_rejects_a_task_that_is_not_queued() {
        let queue = tasks(2);
        let err = select(
            &queue,
            &states(&[(1, TaskState::Done), (2, failed())]),
            Some(TaskId::new(2)),
            None,
        )
        .expect_err("a failed task is not runnable");
        assert!(err.contains("task 2 is Failed"), "got {err}");
    }

    #[test]
    fn select_task_rejects_a_task_behind_an_unpublished_predecessor() {
        let queue = tasks(2);
        let err = select(
            &queue,
            &states(&[(1, TaskState::Queued), (2, TaskState::Queued)]),
            Some(TaskId::new(2)),
            None,
        )
        .expect_err("task 1 has not been published");
        assert!(err.contains("predecessor 1"), "got {err}");
    }

    #[test]
    fn select_task_treats_a_task_with_no_journaled_state_as_queued() {
        let queue = tasks(1);
        let (scope, _) = select(&queue, &BTreeMap::new(), Some(TaskId::new(1)), None)
            .expect("an untouched task is queued");
        assert_eq!(scope, vec![task(1)]);
    }

    // ---- blocked_outcome ----

    #[test]
    fn a_settled_queue_is_not_blocked() {
        let queue = tasks(2);
        let states = states(&[(1, TaskState::Done), (2, TaskState::Cancelled)]);
        assert_eq!(blocked_outcome(&queue, None, &states), None);
    }

    #[test]
    fn a_failed_task_blocks_the_queue_as_a_failure() {
        let queue = tasks(3);
        let states = states(&[(1, TaskState::Done), (2, failed()), (3, TaskState::Queued)]);
        assert_eq!(
            blocked_outcome(&queue, None, &states),
            Some(RunOutcome::TaskFailed {
                task: TaskId::new(2)
            })
        );
    }

    #[test]
    fn each_pause_reason_maps_to_its_own_outcome() {
        let queue = tasks(1);
        let until = OffsetDateTime::UNIX_EPOCH;
        let cases = [
            (
                PauseReason::HumanGate,
                RunOutcome::HumanGate {
                    task: TaskId::new(1),
                },
            ),
            (
                PauseReason::Input,
                RunOutcome::NeedsInput {
                    task: TaskId::new(1),
                },
            ),
            (
                PauseReason::Limit { until: Some(until) },
                RunOutcome::ProviderLimit { until: Some(until) },
            ),
            (PauseReason::Interrupted, RunOutcome::Interrupted),
            (PauseReason::Blocked, RunOutcome::Interrupted),
        ];
        for (reason, expected) in cases {
            let states = states(&[(1, paused(reason.clone()))]);
            assert_eq!(
                blocked_outcome(&queue, None, &states),
                Some(expected),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn a_task_left_mid_attempt_blocks_the_queue_as_resumable() {
        let queue = tasks(2);
        let in_flight = [
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
        ];
        for state in in_flight {
            let states = states(&[(1, state.clone()), (2, TaskState::Queued)]);
            assert_eq!(
                blocked_outcome(&queue, None, &states),
                Some(RunOutcome::Interrupted),
                "{state:?}"
            );
        }
    }

    #[test]
    fn published_and_acknowledged_tasks_do_not_block_the_queue() {
        let queue = tasks(2);
        let states = states(&[
            (
                1,
                TaskState::PublishedVerified {
                    commit: "abc".to_string(),
                },
            ),
            (
                2,
                TaskState::Acknowledged {
                    by: "me".to_string(),
                    at: OffsetDateTime::UNIX_EPOCH,
                },
            ),
        ]);
        assert_eq!(blocked_outcome(&queue, None, &states), None);
    }

    #[test]
    fn blocked_outcome_ignores_tasks_before_from() {
        let queue = tasks(2);
        let states = states(&[(1, failed()), (2, TaskState::Done)]);
        assert_eq!(blocked_outcome(&queue, Some(TaskId::new(2)), &states), None);
        assert_eq!(
            blocked_outcome(&queue, None, &states),
            Some(RunOutcome::TaskFailed {
                task: TaskId::new(1)
            })
        );
    }

    // ---- result_of ----

    #[test]
    fn a_done_task_reports_its_short_commit() {
        let result = result_of(&event(
            Some(3),
            EventKind::TaskDone {
                commit: "0123456789abcdef".to_string(),
            },
        ))
        .expect("TaskDone is a result");
        assert_eq!(result.line(), "task 3: done (commit 0123456)");
    }

    #[test]
    fn a_failed_task_reports_its_class_and_detail() {
        let result = result_of(&event(
            Some(2),
            EventKind::TaskFailed {
                class: FailureClass::VerificationFailure,
                detail: "tests red".to_string(),
            },
        ))
        .expect("TaskFailed is a result");
        assert_eq!(
            result.line(),
            "task 2: failed (VerificationFailure: tests red)"
        );
    }

    #[test]
    fn a_pause_and_an_interrupt_are_results_that_are_not_failures() {
        let pause = result_of(&event(
            Some(1),
            EventKind::Paused {
                reason: PauseReason::HumanGate,
            },
        ))
        .expect("Paused is a result");
        assert_eq!(pause.result, "paused");
        assert!(pause.line().contains("human gate"), "{}", pause.line());

        let interrupt = result_of(&event(
            Some(1),
            EventKind::Interrupted {
                phase: Phase::Implement,
            },
        ))
        .expect("Interrupted is a result");
        assert_eq!(interrupt.line(), "task 1: interrupted (during Implement)");
    }

    #[test]
    fn events_that_do_not_settle_a_task_are_not_results() {
        let not_results = [
            event(Some(1), EventKind::PreflightStarted),
            event(
                Some(1),
                EventKind::VerifyPassed {
                    attempt: AttemptId::new(1),
                },
            ),
            event(Some(1), EventKind::Resumed),
        ];
        for e in not_results {
            assert_eq!(result_of(&e), None, "{:?}", e.kind);
        }
    }

    #[test]
    fn an_event_with_no_task_is_never_a_result() {
        let e = event(
            None,
            EventKind::TaskDone {
                commit: "abc".to_string(),
            },
        );
        assert_eq!(result_of(&e), None);
    }

    #[test]
    fn a_result_serializes_to_json_without_an_absent_detail() {
        let with = TaskResult {
            task: TaskId::new(1),
            result: "done",
            detail: Some("commit abc".to_string()),
        };
        let without = TaskResult {
            task: TaskId::new(2),
            result: "failed",
            detail: None,
        };
        assert_eq!(
            serde_json::to_value(&with).expect("serialize"),
            serde_json::json!({"task": 1, "result": "done", "detail": "commit abc"})
        );
        assert_eq!(
            serde_json::to_value(&without).expect("serialize"),
            serde_json::json!({"task": 2, "result": "failed"})
        );
    }

    // ---- progress_line ----

    #[test]
    fn agent_output_is_prefixed_line_by_line() {
        let line = progress_line(&event(
            Some(4),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "one\ntwo\n".to_string(),
            },
        ));
        assert_eq!(line.as_deref(), Some("task 4 | one\ntask 4 | two"));
    }

    #[test]
    fn blank_agent_output_is_not_shown() {
        let line = progress_line(&event(
            Some(4),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stderr,
                text: "  \n".to_string(),
            },
        ));
        assert_eq!(line, None);
    }

    #[test]
    fn progress_names_the_task_or_the_run() {
        let task_scoped = progress_line(&event(
            Some(2),
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            },
        ));
        assert_eq!(task_scoped.as_deref(), Some("task 2: attempt 1: phase Red"));

        let run_scoped = progress_line(&event(None, EventKind::PreflightStarted));
        assert_eq!(
            run_scoped.as_deref(),
            Some("run: preflight: checking the repository")
        );
    }

    #[test]
    fn a_task_decision_reads_as_one_recovery_line() {
        let decision = RecoveryDecision::Task {
            task: TaskId::new(3),
            decision: ktask_core::Recovery::Resume,
            detail: "the process\nis gone".to_string(),
        };

        assert_eq!(
            recovery_line(&decision),
            "recovery: task 3: Resume: the process is gone"
        );
    }

    #[test]
    fn a_rebuilt_projection_is_reported_as_a_recovery_line() {
        let line = recovery_line(&RecoveryDecision::StateRebuilt);

        assert!(line.starts_with("recovery: "), "got {line}");
        assert!(line.contains("rebuilt"), "got {line}");
    }

    #[test]
    fn progress_covers_the_attempt_lifecycle() {
        let attempt = AttemptId::new(2);
        let cases = [
            (
                EventKind::RetryStarted { attempt },
                "task 1: retry: attempt 2 started in a fresh session",
            ),
            (
                EventKind::PreflightPassed {
                    base_sha: "0123456789".to_string(),
                },
                "task 1: preflight: passed (base 0123456)",
            ),
            (
                EventKind::PreflightFailed {
                    class: FailureClass::EnvironmentFailure,
                    detail: "disk full".to_string(),
                },
                "task 1: preflight: failed (EnvironmentFailure): disk full",
            ),
            (
                EventKind::AttemptStarted {
                    attempt,
                    protocol: "tdd".to_string(),
                    pid: 1,
                    base_sha: "abc".to_string(),
                },
                "task 1: attempt 2 started (tdd)",
            ),
            (
                EventKind::AttemptFinished {
                    attempt,
                    exit_code: 3,
                    usage: None,
                    session_id: None,
                    model_reported: None,
                },
                "task 1: attempt 2 finished (exit 3)",
            ),
            (
                EventKind::VerifyPassed { attempt },
                "task 1: attempt 2: verified",
            ),
            (
                EventKind::VerifyFailed {
                    attempt,
                    class: FailureClass::VerificationFailure,
                    detail: "red".to_string(),
                },
                "task 1: attempt 2: verification failed (VerificationFailure): red",
            ),
            (
                EventKind::PublishStarted {
                    attempt,
                    candidate_sha: "fedcba9876".to_string(),
                },
                "task 1: publishing fedcba9",
            ),
            (
                EventKind::PublishVerified {
                    commit: "fedcba9876".to_string(),
                    remote_sha: "fedcba9876".to_string(),
                },
                "task 1: published fedcba9 (confirmed on the remote)",
            ),
            (
                EventKind::TaskFailed {
                    class: FailureClass::AgentFailure,
                    detail: "gave up".to_string(),
                },
                "task 1: failed (AgentFailure): gave up",
            ),
            (
                EventKind::TaskCancelled {
                    reason: "no longer needed".to_string(),
                },
                "task 1: cancelled: no longer needed",
            ),
            (EventKind::Resumed, "task 1: resumed"),
            (
                EventKind::Interrupted {
                    phase: Phase::Green,
                },
                "task 1: interrupted during Green",
            ),
            (
                EventKind::Paused {
                    reason: PauseReason::Input,
                },
                "task 1: paused: needs input; `ktask-rs resolve` answers it",
            ),
            (
                EventKind::DecisionResolved {
                    adr_path: std::path::PathBuf::from("docs/adr/0009-storage.md"),
                    answer: "SQLite".to_string(),
                },
                "task 1: decision resolved: docs/adr/0009-storage.md",
            ),
        ];
        for (kind, expected) in cases {
            let shown = progress_line(&event(Some(1), kind.clone()));
            assert_eq!(shown.as_deref(), Some(expected), "{kind:?}");
        }
    }

    #[test]
    fn a_finished_task_is_announced_on_stderr_as_well() {
        let shown = progress_line(&event(
            Some(1),
            EventKind::TaskDone {
                commit: "abc".to_string(),
            },
        ));
        assert_eq!(shown.as_deref(), Some("task 1: done"));
    }

    #[test]
    fn internal_bookkeeping_events_are_not_shown() {
        let hidden = [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::GateAcknowledged {
                by: "me".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
        ];
        for kind in hidden {
            assert_eq!(
                progress_line(&event(Some(1), kind.clone())),
                None,
                "{kind:?}"
            );
        }
    }

    // ---- text helpers ----

    #[test]
    fn short_takes_seven_characters_and_tolerates_less() {
        assert_eq!(short("0123456789"), "0123456");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn one_line_folds_whitespace_and_truncates_long_text() {
        assert_eq!(one_line("a\n  b\tc"), "a b c");
        let long = "x".repeat(DETAIL_LIMIT + 50);
        let cut = one_line(&long);
        assert_eq!(cut.chars().count(), DETAIL_LIMIT + 1);
        assert!(cut.ends_with('…'));
        let exact = "y".repeat(DETAIL_LIMIT);
        assert_eq!(one_line(&exact), exact, "text at the limit is untouched");
    }

    #[test]
    fn a_pause_is_described_with_its_remedy() {
        let until = OffsetDateTime::UNIX_EPOCH;
        assert!(pause_text(&PauseReason::HumanGate).contains("ktask-rs ack"));
        assert!(pause_text(&PauseReason::Input).contains("ktask-rs resolve"));
        assert!(pause_text(&PauseReason::Limit { until: Some(until) }).contains("1970"));
        assert_eq!(
            pause_text(&PauseReason::Limit { until: None }),
            "provider limit reached"
        );
        assert_eq!(pause_text(&PauseReason::Interrupted), "interrupted");
        assert_eq!(pause_text(&PauseReason::Blocked), "blocked");
    }

    #[test]
    fn every_outcome_has_a_closing_sentence_naming_what_to_do() {
        let until = OffsetDateTime::UNIX_EPOCH;
        let task = TaskId::new(5);
        assert_eq!(describe(&RunOutcome::Drained), "queue drained");
        assert!(describe(&RunOutcome::TaskFailed { task }).contains("task 5 failed"));
        assert!(describe(&RunOutcome::HumanGate { task }).contains("ktask-rs ack"));
        assert!(describe(&RunOutcome::NeedsInput { task }).contains("ktask-rs resolve --task 5"));
        assert!(describe(&RunOutcome::Interrupted).contains("ktask-rs resume"));
        assert!(describe(&RunOutcome::ProviderLimit { until: Some(until) }).contains("until 1970"));
        assert!(describe(&RunOutcome::ProviderLimit { until: None }).contains("ktask-rs resume"));
        assert_eq!(
            describe(&RunOutcome::Usage {
                detail: "bad".to_string()
            }),
            "bad"
        );
        assert_eq!(
            describe(&RunOutcome::CheckFailed {
                detail: "worse".to_string()
            }),
            "worse"
        );
    }
}

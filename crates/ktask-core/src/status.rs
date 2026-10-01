//! `status`: what ran and how it ended, for every task that has been attempted.

use std::time::Duration;

use crate::{
    Clock, Journal, JournalError, Outcome, RunLock, Task, TaskId, TaskStatus, list_all_tasks,
};

mod lines;
mod outcome;

use lines::{running_step, step_outcome, step_provider};
pub use outcome::AttemptOutcome;

/// The step kind that runs an agent on the task's whole prompt.
pub const IMPLEMENTATION: &str = "implementation";

/// The step kind that runs an agent in the reviewer role on the implementation step's diff,
/// after it.
pub const REVIEW_STEP: &str = "review";

/// The step kind that runs an agent in the tester role on the implementation step's diff,
/// after the review step.
pub const TEST_STEP: &str = "testing";

/// The step kind that pulls the project's tracked branch with rebase before the health
/// check.
pub const SYNC_STEP: &str = "sync";

/// The step kind that runs the project's configured health-check command before the
/// implementation step.
pub const HEALTH_CHECK_STEP: &str = "health check";

/// The step kind that commits everything the task's attempt changed, once the test step has
/// passed.
pub const COMMIT_STEP: &str = "commit";

/// The step kind that pushes the commit step's commit to the project's tracked branch, once it
/// has made one, and confirms the remote branch's tip is that commit.
pub const PUSH_STEP: &str = "push";

/// One step of an attempt, as `status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepLine {
    /// The step's name.
    pub step: String,
    /// The provider that ran it, for a step an agent runs — the implementation, review and
    /// test steps. `None` for a step the tool runs itself — the sync, health check, commit
    /// and push steps, today — which names no provider because none had anything to do with
    /// it.
    pub provider: Option<String>,
    /// How long it has run: the recorded duration once it has ended, elapsed time so far
    /// while it is running.
    pub time_spent: Duration,
    /// What it ended at, or that it is still running.
    pub outcome: AttemptOutcome,
    /// Why, when the outcome is not a success.
    pub reason: Option<String>,
}

/// One task's attempt, as `status` shows it. `step`, `time_spent`, `outcome` and `reason`
/// carry the most recently started or ended step — the same single line `status` and the
/// queue screen showed before an attempt could run more than one — and `steps` carries every
/// step run so far, in order, for a caller that wants the full history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLine {
    /// The attempt's number.
    pub number: u32,
    /// The name of its most recent step.
    pub step: String,
    /// The provider that ran the most recent step, when it is run by an agent; `None` when
    /// that step is run by the tool itself.
    pub provider: Option<String>,
    /// How long the most recent step has run: the recorded duration once it has ended,
    /// elapsed time so far while it is running.
    pub time_spent: Duration,
    /// What the most recent step ended at, or that it is still running.
    pub outcome: AttemptOutcome,
    /// Why, when the most recent step's outcome is not a success.
    pub reason: Option<String>,
    /// Every step run so far, in the order they were started.
    pub steps: Vec<StepLine>,
}

/// One task as `status` shows it: not pending, with its most recent attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The task's number.
    pub task: TaskId,
    /// The task's title.
    pub title: String,
    /// The task's current status.
    pub status: TaskStatus,
    /// Its most recent attempt.
    pub attempt: AttemptLine,
    /// Every earlier attempt, oldest first — every one it was retried past. Empty for a task
    /// still on its first attempt.
    pub history: Vec<AttemptLine>,
}

/// The status word to show for a task: `status.as_str()`, except `"interrupted"` when its
/// most recent attempt's outcome is [`AttemptOutcome::Interrupted`] — a task the journal still
/// calls `running`, but whose run is not alive to finish it. `status` and the queue screen
/// both show this word in place of the task's own status, from this one place, so neither
/// decides it on its own.
#[must_use]
pub fn displayed_status(status: TaskStatus, outcome: Option<AttemptOutcome>) -> &'static str {
    if outcome == Some(AttemptOutcome::Interrupted) {
        "interrupted"
    } else {
        status.as_str()
    }
}

/// The [`StatusEntry`] for `task`, given its most recent attempt and the agent's own report of
/// it, when there was one; `run_alive` says whether a live run currently holds the project's
/// run lock, which only matters when the attempt's last step has not ended.
///
/// One [`StepLine`] is built per step the journal recorded, in order. When no step has been
/// recorded at all — the attempt itself was ended directly, as a run killed outright and never
/// reconciled leaves it, before ever starting one — the attempt's own record is shown instead,
/// under the pipeline's first step name.
/// One line per step of `attempt`, folded from its own recorded outcome — never the whole
/// attempt's most recent one, which a later report-driven step (review, after implementation)
/// would otherwise overwrite here.
fn step_lines(
    attempt: &crate::Attempt,
    clock: &impl Clock,
    run_alive: bool,
    answer: Option<&str>,
) -> Vec<StepLine> {
    let provider = attempt.provider.as_deref();
    attempt
        .steps
        .iter()
        .map(|step| match &step.ended {
            Some(end) => {
                let own_report = end.reported.map(|outcome| (outcome, end.reason.clone()));
                let (outcome, reason) = step_outcome(&step.name, end, own_report, answer);
                StepLine {
                    step: step.name.clone(),
                    provider: step_provider(&step.name, provider),
                    time_spent: end.duration,
                    outcome,
                    reason,
                }
            }
            None => running_step(&step.name, provider, step.started_at, clock, run_alive),
        })
        .collect()
}

/// The current step line: the last of `steps` when there is one, else a fallback for the
/// implementation step itself — pushed onto `steps` too, so it is never missing from what
/// [`entry_for`] records.
fn current_step_line(
    steps: &mut Vec<StepLine>,
    attempt: &crate::Attempt,
    reported: Option<(Outcome, Option<String>)>,
    clock: &impl Clock,
    run_alive: bool,
    answer: Option<&str>,
) -> StepLine {
    if let Some(last) = steps.last() {
        return last.clone();
    }
    let fallback = match &attempt.ended {
        Some(end) => {
            let (outcome, reason) = step_outcome(IMPLEMENTATION, end, reported, answer);
            StepLine {
                step: IMPLEMENTATION.to_owned(),
                provider: step_provider(IMPLEMENTATION, attempt.provider.as_deref()),
                time_spent: end.duration,
                outcome,
                reason,
            }
        }
        None => running_step(
            IMPLEMENTATION,
            attempt.provider.as_deref(),
            attempt.started_at,
            clock,
            run_alive,
        ),
    };
    steps.push(fallback.clone());
    fallback
}

/// `attempt` as an [`AttemptLine`]: every step it has run so far, and the most recently started
/// or ended one's own fields carried flat, given the agent's own report of it, when there was
/// one; `run_alive` only matters for the attempt currently open, never for an earlier one in a
/// task's history, which has always ended.
fn attempt_line(
    attempt: &crate::Attempt,
    reported: Option<(Outcome, Option<String>)>,
    clock: &impl Clock,
    run_alive: bool,
    answer: Option<&str>,
) -> AttemptLine {
    let mut steps = step_lines(attempt, clock, run_alive, answer);
    let current = current_step_line(&mut steps, attempt, reported, clock, run_alive, answer);
    AttemptLine {
        number: attempt.number,
        step: current.step,
        provider: current.provider,
        time_spent: current.time_spent,
        outcome: current.outcome,
        reason: current.reason,
        steps,
    }
}

fn entry_for(
    task: Task,
    attempt: &crate::Attempt,
    reported: Option<(Outcome, Option<String>)>,
    history: Vec<AttemptLine>,
    clock: &impl Clock,
    run_alive: bool,
    answer: Option<&str>,
) -> StatusEntry {
    StatusEntry {
        task: task.id,
        title: task.title,
        status: task.status,
        attempt: attempt_line(attempt, reported, clock, run_alive, answer),
        history,
    }
}

/// The [`StatusEntry`] for `task`, given the step and reason a gate recorded stopping it before
/// any attempt began: one [`StepLine`] shown [`AttemptOutcome::Failed`], the same way a
/// command-kind step that fails inside an attempt is shown — `task.status` is untouched, still
/// `pending`.
fn gate_stop_entry(task: Task, step: String, reason: String) -> StatusEntry {
    let line = StepLine {
        step,
        provider: None,
        time_spent: Duration::ZERO,
        outcome: AttemptOutcome::Failed,
        reason: Some(reason),
    };
    StatusEntry {
        task: task.id,
        title: task.title,
        status: task.status,
        attempt: AttemptLine {
            number: 0,
            step: line.step.clone(),
            provider: None,
            time_spent: Duration::ZERO,
            outcome: line.outcome,
            reason: line.reason.clone(),
            steps: vec![line],
        },
        history: Vec::new(),
    }
}

/// Use case: what ran and how it ended, in queue order — one [`StatusEntry`] for every task
/// that was attempted at least once, cancelled tasks included when they were, plus every
/// pending task a sync or health-check gate most recently stopped before its attempt began.
/// The [`StatusEntry`] for `task`, read fresh from `journal`, given `run_alive` — [`status`]'s
/// own per-task work, pulled out of it so it stays within the workspace's function-length
/// limit. `None` when `task` was never attempted and no gate ever stopped it either.
fn entry_for_task(
    journal: &impl Journal,
    task: Task,
    clock: &impl Clock,
    run_alive: bool,
) -> Result<Option<StatusEntry>, JournalError> {
    let mut attempts = crate::attempt::all_attempts(journal, task.id)?;
    let Some(attempt) = attempts.pop() else {
        let gate_stop = if task.status == TaskStatus::Pending {
            crate::attempt::gate_stop_of(journal, task.id)?
        } else {
            None
        };
        return Ok(gate_stop.map(|(step, reason)| gate_stop_entry(task, step, reason)));
    };
    let reported = crate::attempt::last_report(journal, task.id, attempt.number)?;
    let answer = crate::attempt::answer_of(journal, task.id, attempt.number)?;
    let mut history = Vec::with_capacity(attempts.len());
    for earlier in &attempts {
        let reported = crate::attempt::last_report(journal, task.id, earlier.number)?;
        let earlier_answer = crate::attempt::answer_of(journal, task.id, earlier.number)?;
        history.push(attempt_line(
            earlier,
            reported,
            clock,
            false,
            earlier_answer.as_deref(),
        ));
    }
    Ok(Some(entry_for(
        task,
        &attempt,
        reported,
        history,
        clock,
        run_alive,
        answer.as_deref(),
    )))
}

/// A task never attempted, never stopped by a gate, pending or cancelled before it ever ran, is
/// left out.
///
/// A task the journal still calls `running` is shown `running` only while `lock` says a run is
/// actually alive; otherwise — a run that was killed outright left it behind — it is shown
/// `interrupted` at once, with no need to wait for the next `run` to reconcile it.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when `lock` cannot be used.
pub fn status(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
) -> Result<Vec<StatusEntry>, JournalError> {
    let run_alive = match crate::attempt::running(journal)? {
        Some(_) => lock
            .in_progress()
            .map_err(|error| JournalError::new(error.to_string()))?,
        None => false,
    };
    let mut entries = Vec::new();
    for task in list_all_tasks(journal)? {
        if let Some(entry) = entry_for_task(journal, task, clock, run_alive)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::fakes::{FakeClock, FakeJournal, FakeRunLock, at, draft};
    use crate::{AttemptEnd, AttemptRun, Outcome, Placement, TaskId, TaskStatus, add_task, report};

    use super::*;

    fn clock(seconds: u64) -> FakeClock {
        FakeClock(at(seconds))
    }

    /// A lock no run holds.
    fn no_run() -> FakeRunLock {
        FakeRunLock::free()
    }

    /// A lock a live run holds.
    fn a_live_run() -> FakeRunLock {
        FakeRunLock::held_by(Some(4_321))
    }

    #[test]
    fn a_pending_task_is_left_out() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        assert_eq!(status(&journal, &clock(0), &no_run()).unwrap(), vec![]);
    }

    #[test]
    fn a_cancelled_task_that_was_never_attempted_is_left_out() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::remove_task(&journal, &clock(0), TaskId(1)).unwrap();
        assert_eq!(status(&journal, &clock(0), &no_run()).unwrap(), vec![]);
    }

    #[test]
    fn an_empty_queue_has_no_status_and_a_project_with_no_attempts_reports_nothing() {
        assert_eq!(
            status(&FakeJournal::default(), &clock(0), &no_run()).unwrap(),
            vec![]
        );
    }

    /// A journal with one task titled `a`, whose one attempt was started at second 100 and ran
    /// with `echo`.
    fn journal_with_a_started_attempt() -> FakeJournal {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(100), TaskId(1), "echo", None)
            .unwrap();
        journal
    }

    #[test]
    fn a_running_attempt_with_its_run_alive_shows_no_outcome_and_its_elapsed_time_so_far() {
        let journal = journal_with_a_started_attempt();
        let entries = status(&journal, &clock(130), &a_live_run()).unwrap();
        assert_eq!(
            entries,
            vec![StatusEntry {
                task: TaskId(1),
                title: "a".to_owned(),
                status: TaskStatus::Running,
                attempt: AttemptLine {
                    number: 1,
                    step: IMPLEMENTATION.to_owned(),
                    provider: Some("echo".to_owned()),
                    time_spent: Duration::from_secs(30),
                    outcome: AttemptOutcome::Running,
                    reason: None,
                    steps: vec![StepLine {
                        step: IMPLEMENTATION.to_owned(),
                        provider: Some("echo".to_owned()),
                        time_spent: Duration::from_secs(30),
                        outcome: AttemptOutcome::Running,
                        reason: None,
                    }],
                },
                history: vec![],
            }]
        );
    }

    #[test]
    fn a_running_attempt_with_no_run_alive_shows_interrupted_at_once() {
        let journal = journal_with_a_started_attempt();
        let entries = status(&journal, &clock(130), &no_run()).unwrap();
        assert_eq!(
            entries,
            vec![StatusEntry {
                task: TaskId(1),
                title: "a".to_owned(),
                // The task's own persisted status is unaffected: the journal still calls it
                // running, since nothing reconciled it. Only the attempt's shown outcome, and
                // `displayed_status`, say otherwise.
                status: TaskStatus::Running,
                attempt: AttemptLine {
                    number: 1,
                    step: IMPLEMENTATION.to_owned(),
                    provider: Some("echo".to_owned()),
                    time_spent: Duration::from_secs(30),
                    outcome: AttemptOutcome::Interrupted,
                    reason: None,
                    steps: vec![StepLine {
                        step: IMPLEMENTATION.to_owned(),
                        provider: Some("echo".to_owned()),
                        time_spent: Duration::from_secs(30),
                        outcome: AttemptOutcome::Interrupted,
                        reason: None,
                    }],
                },
                history: vec![],
            }]
        );
        assert_eq!(
            displayed_status(entries[0].status, Some(entries[0].attempt.outcome)),
            "interrupted"
        );
    }

    /// The `AttemptLine` of the sole task in a queue built with [`journal_with_a_started_attempt`],
    /// after `outcome` (with `reason`) is reported and the attempt ends at `status`.
    fn attempt_after(
        outcome: Outcome,
        reason: Option<&str>,
        status_at_end: TaskStatus,
    ) -> AttemptLine {
        let journal = journal_with_a_started_attempt();
        report(
            &journal,
            &clock(110),
            &crate::AttemptToken::new("proj", TaskId(1), 1),
            outcome,
            reason,
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(12),
                exit_code: Some(0),
                status: status_at_end,
                reason,
            },
            clock(112).0,
        )
        .unwrap();
        status(&journal, &clock(200), &no_run())
            .unwrap()
            .remove(0)
            .attempt
    }

    #[test]
    fn a_done_report_shows_the_done_outcome_and_the_recorded_duration_not_the_elapsed_time() {
        let attempt = attempt_after(Outcome::Done, None, TaskStatus::Done);
        assert_eq!(attempt.outcome, AttemptOutcome::Reported(Outcome::Done));
        assert_eq!(attempt.reason, None);
        assert_eq!(attempt.time_spent, Duration::from_secs(12));
    }

    #[test]
    fn a_failed_report_shows_the_failed_outcome_and_its_reason() {
        let attempt = attempt_after(Outcome::Failed, Some("it broke"), TaskStatus::Failed);
        assert_eq!(attempt.outcome, AttemptOutcome::Reported(Outcome::Failed));
        assert_eq!(attempt.reason.as_deref(), Some("it broke"));
    }

    #[test]
    fn a_too_large_report_keeps_its_own_label_distinct_from_failed() {
        let attempt = attempt_after(Outcome::TooLarge, Some("split me"), TaskStatus::Failed);
        assert_eq!(attempt.outcome, AttemptOutcome::Reported(Outcome::TooLarge));
        assert_eq!(attempt.outcome.as_str(), "too-large");
        assert_eq!(attempt.reason.as_deref(), Some("split me"));
    }

    #[test]
    fn a_needs_input_report_keeps_its_own_label_even_though_the_task_is_blocked() {
        let journal = journal_with_a_started_attempt();
        report(
            &journal,
            &clock(110),
            &crate::AttemptToken::new("proj", TaskId(1), 1),
            Outcome::NeedsInput,
            Some("which path?"),
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(5),
                exit_code: Some(0),
                status: TaskStatus::Blocked,
                reason: Some("which path?"),
            },
            clock(115).0,
        )
        .unwrap();
        let entries = status(&journal, &clock(200), &no_run()).unwrap();
        assert_eq!(entries[0].status, TaskStatus::Blocked);
        assert_eq!(
            entries[0].attempt.outcome,
            AttemptOutcome::Reported(Outcome::NeedsInput)
        );
        assert_eq!(entries[0].attempt.outcome.as_str(), "needs-input");
        assert_eq!(entries[0].attempt.reason.as_deref(), Some("which path?"));
    }

    #[test]
    fn once_answered_the_blocked_attempts_reason_carries_the_answer_too() {
        let journal = journal_with_a_started_attempt();
        report(
            &journal,
            &clock(110),
            &crate::AttemptToken::new("proj", TaskId(1), 1),
            Outcome::NeedsInput,
            Some("which path?"),
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(5),
                exit_code: Some(0),
                status: TaskStatus::Blocked,
                reason: Some("which path?"),
            },
            clock(115).0,
        )
        .unwrap();
        crate::answer_task(&journal, &clock(120), TaskId(1), "the left one").unwrap();

        let entries = status(&journal, &clock(200), &no_run()).unwrap();
        assert_eq!(entries[0].status, TaskStatus::Pending);
        assert_eq!(
            entries[0].attempt.reason.as_deref(),
            Some("which path? — answer: the left one")
        );
        let step = entries[0]
            .attempt
            .steps
            .iter()
            .find(|step| step.step == IMPLEMENTATION)
            .expect("the implementation step");
        assert_eq!(
            step.reason.as_deref(),
            Some("which path? — answer: the left one")
        );
    }

    #[test]
    fn no_report_at_all_shows_the_tools_own_failed_unknown_outcome_and_reason() {
        let journal = journal_with_a_started_attempt();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(7),
                exit_code: Some(0),
                status: TaskStatus::FailedUnknown,
                reason: Some("the provider exited with code 0 and reported nothing"),
            },
            clock(107).0,
        )
        .unwrap();
        let entries = status(&journal, &clock(200), &no_run()).unwrap();
        assert_eq!(entries[0].status, TaskStatus::FailedUnknown);
        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Unreported);
        assert_eq!(entries[0].attempt.outcome.as_str(), "failed-unknown");
        assert_eq!(
            entries[0].attempt.reason.as_deref(),
            Some("the provider exited with code 0 and reported nothing")
        );
        assert_eq!(entries[0].attempt.time_spent, Duration::from_secs(7));
    }

    #[test]
    fn a_task_waiting_to_be_retried_is_shown_pending_with_its_ended_attempt() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(1),
                exit_code: Some(1),
                status: TaskStatus::Failed,
                reason: Some("it broke"),
            },
            clock(1).0,
        )
        .unwrap();
        crate::retry_task(&journal, &clock(2), TaskId(1)).unwrap();

        let entries = status(&journal, &clock(3), &no_run()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, TaskStatus::Pending);
        assert_eq!(entries[0].attempt.number, 1);
        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Unreported);
        assert_eq!(entries[0].history, vec![]);
    }

    #[test]
    fn a_retried_tasks_second_attempt_shows_its_first_as_history_under_it() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(1),
                exit_code: Some(1),
                status: TaskStatus::Failed,
                reason: Some("it broke"),
            },
            clock(1).0,
        )
        .unwrap();
        crate::retry_task(&journal, &clock(2), TaskId(1)).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(3), TaskId(1), "echo", None)
            .unwrap();

        let entries = status(&journal, &clock(10), &a_live_run()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, TaskStatus::Running);
        assert_eq!(entries[0].attempt.number, 2);
        assert_eq!(entries[0].history.len(), 1, "{:?}", entries[0].history);
        assert_eq!(entries[0].history[0].number, 1);
        assert_eq!(entries[0].history[0].outcome, AttemptOutcome::Unreported);
        assert_eq!(entries[0].history[0].reason.as_deref(), Some("it broke"));
    }

    #[test]
    fn entries_are_in_queue_order_and_pending_tasks_are_skipped_in_between() {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(0), &draft(title), Placement::End).unwrap();
        }
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        report(
            &journal,
            &clock(1),
            &crate::AttemptToken::new("proj", TaskId(1), 1),
            Outcome::Done,
            None,
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(1),
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            at(1),
        )
        .unwrap();
        // task 2 stays pending.
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(3), "echo", None)
            .unwrap();
        report(
            &journal,
            &clock(1),
            &crate::AttemptToken::new("proj", TaskId(3), 1),
            Outcome::Failed,
            Some("nope"),
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(3),
            1,
            AttemptRun {
                duration: Duration::from_secs(1),
                exit_code: Some(0),
                status: TaskStatus::Failed,
                reason: Some("nope"),
            },
            at(1),
        )
        .unwrap();

        let entries = status(&journal, &clock(2), &no_run()).unwrap();
        let ids: Vec<_> = entries.iter().map(|entry| entry.task).collect();
        assert_eq!(ids, [TaskId(1), TaskId(3)]);
        assert_eq!(entries[0].status, TaskStatus::Done);
        assert_eq!(entries[1].status, TaskStatus::Failed);
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(status(&journal, &clock(0), &no_run()), Err(failure));
    }

    #[test]
    fn outcome_labels_read_as_expected() {
        assert_eq!(AttemptOutcome::Running.as_str(), "running");
        assert_eq!(AttemptOutcome::Reported(Outcome::Done).as_str(), "done");
        assert_eq!(AttemptOutcome::Unreported.as_str(), "failed-unknown");
        assert_eq!(AttemptOutcome::Interrupted.as_str(), "interrupted");
        assert_eq!(AttemptOutcome::Passed.as_str(), "passed");
        assert_eq!(AttemptOutcome::Failed.as_str(), "failed");
        assert_eq!(AttemptOutcome::Running.to_string(), "running");
    }

    #[test]
    fn a_command_kind_step_that_did_not_pass_is_shown_failed_with_its_reason() {
        let end = AttemptEnd {
            duration: Duration::from_secs(1),
            status: TaskStatus::Failed,
            reason: Some("git identity is not configured".to_owned()),
            reported: None,
        };
        assert_eq!(
            step_outcome(COMMIT_STEP, &end, None, None),
            (
                AttemptOutcome::Failed,
                Some("git identity is not configured".to_owned())
            )
        );
    }

    #[test]
    fn displayed_status_only_overrides_a_task_shown_interrupted() {
        assert_eq!(
            displayed_status(TaskStatus::Running, Some(AttemptOutcome::Interrupted)),
            "interrupted"
        );
        assert_eq!(
            displayed_status(TaskStatus::Running, Some(AttemptOutcome::Running)),
            "running"
        );
        assert_eq!(displayed_status(TaskStatus::Running, None), "running");
        assert_eq!(
            displayed_status(
                TaskStatus::Failed,
                Some(AttemptOutcome::Reported(Outcome::TooLarge))
            ),
            "failed"
        );
    }

    #[test]
    fn a_health_check_step_that_already_passed_is_its_own_line_ahead_of_the_current_one() {
        let journal = journal_with_a_started_attempt();
        crate::attempt::begin_step(&journal, &clock(100), TaskId(1), 1, HEALTH_CHECK_STEP).unwrap();
        crate::attempt::end_step(
            &journal,
            &clock(104),
            TaskId(1),
            1,
            HEALTH_CHECK_STEP,
            AttemptRun {
                duration: Duration::from_secs(4),
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            None,
        )
        .unwrap();
        crate::attempt::begin_step(&journal, &clock(104), TaskId(1), 1, IMPLEMENTATION).unwrap();

        let entries = status(&journal, &clock(110), &a_live_run()).unwrap();
        assert_eq!(
            entries[0].attempt.steps,
            vec![
                StepLine {
                    step: HEALTH_CHECK_STEP.to_owned(),
                    // The health check is run by the tool itself, not the agent: it names no
                    // provider, even though the attempt ran with `echo`.
                    provider: None,
                    time_spent: Duration::from_secs(4),
                    outcome: AttemptOutcome::Passed,
                    reason: None,
                },
                StepLine {
                    step: IMPLEMENTATION.to_owned(),
                    provider: Some("echo".to_owned()),
                    time_spent: Duration::from_secs(6),
                    outcome: AttemptOutcome::Running,
                    reason: None,
                },
            ]
        );
        assert_eq!(entries[0].attempt.step, IMPLEMENTATION);
        assert_eq!(entries[0].attempt.provider.as_deref(), Some("echo"));
    }

    /// Appends a [`crate::Event::GateFailed`] for task `id`, naming `step` and `reason`, at
    /// second 5.
    fn fail_gate(journal: &FakeJournal, id: TaskId, step: &str, reason: &str) {
        let read = journal.events().unwrap().len();
        journal
            .append_events(
                &[crate::Event::GateFailed {
                    id,
                    step: step.to_owned(),
                    reason: reason.to_owned(),
                    at: at(5),
                }],
                read,
            )
            .unwrap();
    }

    #[test]
    fn a_gate_stop_shows_for_a_pending_task_with_no_attempt() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        fail_gate(
            &journal,
            TaskId(1),
            SYNC_STEP,
            "uncommitted changes; commit or stash",
        );

        let entries = status(&journal, &clock(10), &no_run()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task, TaskId(1));
        // The task itself is still pending: a gate stop is not an attempt.
        assert_eq!(entries[0].status, TaskStatus::Pending);
        assert_eq!(
            displayed_status(entries[0].status, Some(entries[0].attempt.outcome)),
            "pending"
        );
        assert_eq!(entries[0].attempt.number, 0);
        assert_eq!(entries[0].attempt.step, SYNC_STEP);
        assert_eq!(entries[0].attempt.provider, None);
        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Failed);
        assert_eq!(
            entries[0].attempt.reason.as_deref(),
            Some("uncommitted changes; commit or stash")
        );
        assert_eq!(
            entries[0].attempt.steps,
            vec![StepLine {
                step: SYNC_STEP.to_owned(),
                provider: None,
                time_spent: Duration::ZERO,
                outcome: AttemptOutcome::Failed,
                reason: Some("uncommitted changes; commit or stash".to_owned()),
            }]
        );
    }

    #[test]
    fn a_gate_stop_is_left_out_once_the_task_it_stopped_is_cancelled() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        fail_gate(&journal, TaskId(1), HEALTH_CHECK_STEP, "exited with code 1");
        crate::remove_task(&journal, &clock(10), TaskId(1)).unwrap();

        assert_eq!(status(&journal, &clock(20), &no_run()).unwrap(), vec![]);
    }
}

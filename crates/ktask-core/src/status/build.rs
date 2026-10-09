//! Builds the typed status facts that are projected from journal history.

use std::time::Duration;

use crate::{
    AttemptOutput, Clock, IMPLEMENTATION, Journal, JournalError, Outcome, Task, TaskId, TaskStatus,
};

use super::AttemptOutcome;
use super::facts::{AttemptLine, DoneMark, OutputActivity, StatusEntry, StepLine};
use super::lines::{
    StepIdentity, attempt_of, gate_stop_entry, running_step, step_outcome, step_provider,
    step_session,
};

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
    journal: &impl Journal,
    task: TaskId,
    attempt: &crate::Attempt,
    clock: &(impl Clock + ?Sized),
    run_alive: bool,
    answer: Option<&str>,
) -> Result<Vec<StepLine>, JournalError> {
    let provider = attempt.provider.as_deref();
    let session = attempt.session.as_deref();
    attempt
        .steps
        .iter()
        .map(|step| match &step.ended {
            Some(end) => ended_line_for(journal, task, attempt, step, end, provider, answer),
            None => Ok(running_step(
                StepIdentity {
                    name: &step.name,
                    provider: step.provider.as_deref().or(provider),
                    model: step.model.as_deref(),
                    session,
                },
                step.started_at,
                attempt.waiting_until,
                attempt.waiting_reason,
                clock,
                run_alive,
            )),
        })
        .collect()
}

/// One already-ended step of `attempt` as a [`StepLine`], its own findings read back from the
/// journal — [`step_lines`]'s own per-step work for a step that has ended, pulled out of it so
/// it stays within the workspace's function-length limit.
fn ended_line_for(
    journal: &impl Journal,
    task: TaskId,
    attempt: &crate::Attempt,
    step: &crate::Step,
    end: &crate::AttemptEnd,
    provider: Option<&str>,
    answer: Option<&str>,
) -> Result<StepLine, JournalError> {
    let findings = crate::attempt::findings_of_step(journal, task, attempt.number, &step.name)?;
    Ok(ended_step_line(
        StepIdentity {
            name: &step.name,
            provider: step.provider.as_deref().or(provider),
            model: step.model.as_deref(),
            session: attempt.session.as_deref(),
        },
        end,
        end.reported.map(|outcome| (outcome, end.reason.clone())),
        answer,
        findings,
    ))
}

/// A recorded provider or command step as one status line. A provider-reported model wins over
/// the requested one because it describes what actually ran.
fn ended_step_line(
    identity: StepIdentity<'_>,
    end: &crate::AttemptEnd,
    reported: Option<(Outcome, Option<String>)>,
    answer: Option<&str>,
    findings: Vec<crate::Finding>,
) -> StepLine {
    let (outcome, reason) = step_outcome(identity.name, end, reported, answer);
    StepLine {
        step: identity.name.to_owned(),
        provider: step_provider(identity.name, identity.provider),
        model: end
            .used_model
            .clone()
            .or_else(|| identity.model.map(str::to_owned)),
        session: step_session(identity.name, identity.session),
        time_spent: end.duration,
        outcome,
        reason,
        findings,
        waiting: None,
        limit_wait: end.limit_wait,
        limit_warning: end.limit_warning.clone(),
        usage: end.usage,
        routed: end.routed,
        more_time: None,
    }
}

/// The current step line: the last of `steps` when there is one, else a fallback for the
/// implementation step itself — pushed onto `steps` too, so it is never missing from what
/// [`entry_for`] records.
fn current_step_line(
    steps: &mut Vec<StepLine>,
    attempt: &crate::Attempt,
    reported: Option<(Outcome, Option<String>)>,
    clock: &(impl Clock + ?Sized),
    run_alive: bool,
    answer: Option<&str>,
) -> StepLine {
    if let Some(last) = steps.last() {
        return last.clone();
    }
    let identity = StepIdentity {
        name: IMPLEMENTATION,
        provider: attempt.provider.as_deref(),
        model: None,
        session: attempt.session.as_deref(),
    };
    let fallback = match &attempt.ended {
        Some(end) => ended_step_line(identity, end, reported, answer, Vec::new()),
        None => running_step(
            identity,
            attempt.started_at,
            attempt.waiting_until,
            attempt.waiting_reason,
            clock,
            run_alive,
        ),
    };
    steps.push(fallback.clone());
    fallback
}

/// `attempt` as an [`AttemptLine`]: every step it has run so far, and the most recently started
/// or ended one's own fields carried flat, given the agent's own report of it, when there was
/// one; `current.run_alive` only matters for the attempt currently open, never for an earlier
/// one in a task's history, which has always ended.
fn attempt_line(
    journal: &impl Journal,
    task: TaskId,
    attempt: &crate::Attempt,
    current: &CurrentAttempt<'_>,
) -> Result<AttemptLine, JournalError> {
    let mut steps = step_lines(
        journal,
        task,
        attempt,
        current.clock,
        current.run_alive,
        current.answer,
    )?;
    let step_current = current_step_line(
        &mut steps,
        attempt,
        current.reported.clone(),
        current.clock,
        current.run_alive,
        current.answer,
    );
    let output_activity = current.output.and_then(|(output, silent_after)| {
        output_activity(
            &step_current,
            attempt,
            current.clock,
            current.run_alive,
            output,
            silent_after,
            task,
        )
    });
    Ok(attempt_of(
        attempt.number,
        step_current,
        output_activity,
        steps,
    ))
}

/// The live output state for this attempt's current step, when that step is an agent provider
/// still actively running. No retained bytes yet is deliberately still silence: operators need
/// to see a provider that started and then said nothing at all.
fn output_activity(
    current: &StepLine,
    attempt: &crate::Attempt,
    clock: &(impl Clock + ?Sized),
    run_alive: bool,
    output: &dyn AttemptOutput,
    silent_after: Duration,
    task: TaskId,
) -> Option<OutputActivity> {
    if !run_alive || current.outcome != AttemptOutcome::Running || current.provider.is_none() {
        return None;
    }
    let last_output_at = output.last_output_at(task, attempt.number);
    let since = last_output_at.unwrap_or_else(|| {
        attempt
            .steps
            .iter()
            .rev()
            .find(|step| step.ended.is_none())
            .map_or(attempt.started_at, |step| step.started_at)
    });
    let silent_for = clock.now().duration_since(since).unwrap_or_default();
    Some(OutputActivity {
        last_output_at,
        active: silent_for < Duration::from_secs(1),
        may_be_stuck: silent_for >= silent_after,
        silent_for,
    })
}

/// The per-current-attempt inputs status uses after the journal has supplied them.
struct CurrentAttempt<'a> {
    reported: Option<(Outcome, Option<String>)>,
    clock: &'a dyn Clock,
    run_alive: bool,
    answer: Option<&'a str>,
    output: Option<(&'a dyn AttemptOutput, Duration)>,
}

/// `task`'s latest `attempt` as its status entry, with the attempt facts already read.
fn entry_for(
    journal: &impl Journal,
    task: Task,
    attempt: &crate::Attempt,
    history: Vec<AttemptLine>,
    current: &CurrentAttempt<'_>,
) -> Result<StatusEntry, JournalError> {
    let id = task.id;
    Ok(StatusEntry {
        task: task.id,
        title: task.title,
        status: task.status,
        attempt: attempt_line(journal, id, attempt, current)?,
        history,
        done_by_user: None,
    })
}

/// Use case: what ran and how it ended, in queue order — one [`StatusEntry`] for every task
/// that was attempted at least once, cancelled tasks included when they were, plus every
/// pending task a sync or health-check gate most recently stopped before its attempt began.
/// The [`StatusEntry`] for `task`, read fresh from `journal`, given `run_alive` — [`status`]'s
/// own per-task work, pulled out of it so it stays within the workspace's function-length
/// limit. `None` when `task` was never attempted and no gate ever stopped it either.
pub(super) fn entry_for_task(
    journal: &impl Journal,
    task: Task,
    clock: &impl Clock,
    run_alive: bool,
    output: Option<&dyn AttemptOutput>,
    silent_after: Duration,
) -> Result<Option<StatusEntry>, JournalError> {
    let mut attempts = crate::attempt::all_attempts(journal, task.id)?;
    let Some(attempt) = attempts.pop() else {
        let gate_stop = if task.status == TaskStatus::Pending {
            crate::attempt::gate_stop_of(journal, task.id)?
        } else {
            None
        };
        return Ok(gate_stop.map(|(step, reason, _at)| gate_stop_entry(task, step, reason)));
    };
    let reported = crate::attempt::last_report(journal, task.id, attempt.number)?;
    let answer = crate::attempt::answer_of(journal, task.id, attempt.number)?;
    let done_by_user =
        crate::attempt::done_mark_of(journal, task.id)?.map(|(reason, at)| DoneMark { reason, at });
    let history = attempt_history(journal, task.id, &attempts, clock)?;
    let previous = attempts.last().map(|previous| previous.number);
    let more_time = more_time_of_next(journal, task.id, previous)?;
    let current = CurrentAttempt {
        reported,
        clock,
        run_alive,
        answer: answer.as_deref(),
        output: output.map(|output| (output, silent_after)),
    };
    let mut entry = entry_for(journal, task, &attempt, history, &current)?;
    entry.done_by_user = done_by_user;
    with_more_time(&mut entry.attempt, more_time);
    Ok(Some(entry))
}

/// The time the resolver's retry decision after attempt `previous` added to the next attempt's
/// limit, when it added any.
fn more_time_of_next(
    journal: &impl Journal,
    id: TaskId,
    previous: Option<u32>,
) -> Result<Option<Duration>, JournalError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    let minutes = crate::attempt::last_retry_more_time(journal, id, previous)?;
    Ok(minutes.map(|minutes| Duration::from_mins(u64::from(minutes))))
}

/// Shows `more_time` on `line` and on the step it applies to, the implementation step.
fn with_more_time(line: &mut AttemptLine, more_time: Option<Duration>) {
    line.more_time = more_time;
    for step in &mut line.steps {
        if step.step == IMPLEMENTATION {
            step.more_time = more_time;
        }
    }
}

/// Every earlier attempt of `attempts` as an [`AttemptLine`], oldest first, given its own
/// report and answer — [`entry_for_task`]'s own work, pulled out of it so it stays within the
/// workspace's function-length limit.
fn attempt_history(
    journal: &impl Journal,
    id: TaskId,
    attempts: &[crate::Attempt],
    clock: &impl Clock,
) -> Result<Vec<AttemptLine>, JournalError> {
    let mut history = Vec::with_capacity(attempts.len());
    for (index, earlier) in attempts.iter().enumerate() {
        let reported = crate::attempt::last_report(journal, id, earlier.number)?;
        let earlier_answer = crate::attempt::answer_of(journal, id, earlier.number)?;
        let current = CurrentAttempt {
            reported,
            clock,
            run_alive: false,
            answer: earlier_answer.as_deref(),
            output: None,
        };
        let mut line = attempt_line(journal, id, earlier, &current)?;
        let previous = index
            .checked_sub(1)
            .and_then(|before| attempts.get(before))
            .map(|before| before.number);
        with_more_time(&mut line, more_time_of_next(journal, id, previous)?);
        history.push(line);
    }
    Ok(history)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use crate::fakes::{FakeClock, FakeJournal, FakeRunLock, at, draft};
    use crate::queue_state::StepEnd;
    use crate::{
        AttemptEnd, AttemptRun, COMMIT_STEP, HEALTH_CHECK_STEP, LimitWait, Outcome, Placement,
        SYNC_STEP, TaskId, TaskStatus, add_task, report, status, status_with_output,
    };

    use super::*;

    fn displayed_status(status: TaskStatus, outcome: Option<AttemptOutcome>) -> &'static str {
        if outcome == Some(AttemptOutcome::Interrupted) {
            "interrupted"
        } else {
            status.as_str()
        }
    }

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

    struct OutputAt(SystemTime);

    impl AttemptOutput for OutputAt {
        fn last_output_at(&self, _task: TaskId, _attempt: u32) -> Option<SystemTime> {
            Some(self.0)
        }
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
                    model: None,
                    session: None,
                    time_spent: Duration::from_secs(30),
                    outcome: AttemptOutcome::Running,
                    reason: None,
                    findings: Vec::new(),
                    waiting: None,
                    limit_wait: None,
                    limit_warning: None,
                    routed: None,
                    more_time: None,
                    usage: crate::Usage::default(),
                    output_activity: None,
                    steps: vec![StepLine {
                        step: IMPLEMENTATION.to_owned(),
                        provider: Some("echo".to_owned()),
                        model: None,
                        session: None,
                        time_spent: Duration::from_secs(30),
                        outcome: AttemptOutcome::Running,
                        reason: None,
                        findings: Vec::new(),
                        waiting: None,
                        limit_wait: None,
                        limit_warning: None,
                        routed: None,
                        more_time: None,
                        usage: crate::Usage::default(),
                    }],
                },
                history: vec![],
                done_by_user: None,
            }]
        );
    }

    #[test]
    fn live_output_is_fresh_then_silent_and_called_out_after_the_threshold() {
        let journal = journal_with_a_started_attempt();
        let fresh = status_with_output(
            &journal,
            &clock(100),
            &a_live_run(),
            &OutputAt(at(100)),
            Duration::from_secs(120),
        )
        .unwrap();
        assert!(fresh[0].attempt.output_activity.as_ref().unwrap().active);
        assert!(
            status_with_output(
                &journal,
                &FakeClock(at(100) + Duration::from_millis(200)),
                &a_live_run(),
                &OutputAt(at(100)),
                Duration::from_secs(120),
            )
            .unwrap()[0]
                .attempt
                .output_activity
                .as_ref()
                .unwrap()
                .active
        );
        let stuck = status_with_output(
            &journal,
            &clock(221),
            &a_live_run(),
            &OutputAt(at(100)),
            Duration::from_secs(120),
        )
        .unwrap();
        let activity = stuck[0].attempt.output_activity.as_ref().unwrap();
        assert_eq!(activity.silent_for, Duration::from_secs(121));
        assert!(activity.may_be_stuck);
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
                    model: None,
                    session: None,
                    time_spent: Duration::from_secs(30),
                    outcome: AttemptOutcome::Interrupted,
                    reason: None,
                    findings: Vec::new(),
                    waiting: None,
                    limit_wait: None,
                    limit_warning: None,
                    routed: None,
                    more_time: None,
                    usage: crate::Usage::default(),
                    output_activity: None,
                    steps: vec![StepLine {
                        step: IMPLEMENTATION.to_owned(),
                        provider: Some("echo".to_owned()),
                        model: None,
                        session: None,
                        time_spent: Duration::from_secs(30),
                        outcome: AttemptOutcome::Interrupted,
                        reason: None,
                        findings: Vec::new(),
                        waiting: None,
                        limit_wait: None,
                        limit_warning: None,
                        routed: None,
                        more_time: None,
                        usage: crate::Usage::default(),
                    }],
                },
                history: vec![],
                done_by_user: None,
            }]
        );
        assert_eq!(
            displayed_status(entries[0].status, Some(entries[0].attempt.outcome)),
            "interrupted"
        );
    }

    #[test]
    fn an_attempt_waiting_on_its_providers_limit_shows_a_live_countdown() {
        let journal = journal_with_a_started_attempt();
        crate::attempt::begin_step(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            IMPLEMENTATION,
            None,
            None,
        )
        .unwrap();
        crate::attempt::record_waiting(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            IMPLEMENTATION,
            at(200),
            crate::WaitReason::UsageLimit,
        )
        .unwrap();

        // At second 130, 70 of the 100 seconds until the reset at second 200 are left — read
        // fresh from the clock, not frozen when the wait began.
        let entries = status(&journal, &clock(130), &a_live_run()).unwrap();
        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Waiting);
        assert_eq!(
            entries[0].attempt.waiting,
            Some(crate::Wait {
                reason: crate::WaitReason::UsageLimit,
                remaining: Duration::from_secs(70),
            })
        );
        assert_eq!(entries[0].attempt.steps.len(), 1);
        assert_eq!(entries[0].attempt.steps[0].outcome, AttemptOutcome::Waiting);

        // A later read, further along, shows less of it left.
        let later = status(&journal, &clock(190), &a_live_run()).unwrap();
        assert_eq!(
            later[0].attempt.waiting.map(|wait| wait.remaining),
            Some(Duration::from_secs(10))
        );

        // A run that is not alive shows interrupted instead, the wait notwithstanding.
        let not_alive = status(&journal, &clock(130), &no_run()).unwrap();
        assert_eq!(not_alive[0].attempt.outcome, AttemptOutcome::Interrupted);
    }

    #[test]
    fn a_transport_retry_wait_shows_its_retry_number_and_the_time_left_to_it() {
        let journal = journal_with_a_started_attempt();
        crate::attempt::begin_step(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            IMPLEMENTATION,
            None,
            None,
        )
        .unwrap();
        let reason = crate::WaitReason::TransportRetry {
            failure: 2,
            limit: 3,
        };
        crate::attempt::record_waiting(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            IMPLEMENTATION,
            at(102),
            reason,
        )
        .unwrap();

        let entries = status(&journal, &clock(101), &a_live_run()).unwrap();
        assert_eq!(
            entries[0].attempt.waiting,
            Some(crate::Wait {
                reason,
                remaining: Duration::from_secs(1),
            })
        );
        assert_eq!(entries[0].attempt.reason, None);
        assert_eq!(entries[0].attempt.steps[0].reason, None);
        let retry_two_of_three = Some(crate::Routed::Retry { n: 2, of: 3 });
        assert_eq!(entries[0].attempt.routed, retry_two_of_three);
        assert_eq!(entries[0].attempt.steps[0].routed, retry_two_of_three);
    }

    #[test]
    fn once_resumed_the_step_that_waited_for_a_limit_still_shows_it_after_it_passes() {
        let journal = journal_with_a_started_attempt();
        crate::attempt::begin_step(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            IMPLEMENTATION,
            None,
            None,
        )
        .unwrap();
        crate::attempt::record_waiting(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            IMPLEMENTATION,
            at(200),
            crate::WaitReason::UsageLimit,
        )
        .unwrap();
        crate::attempt::end_step(
            &journal,
            &clock(205),
            TaskId(1),
            1,
            IMPLEMENTATION,
            StepEnd {
                run: AttemptRun {
                    duration: Duration::from_secs(105),
                    exit_code: Some(0),
                    status: TaskStatus::Done,
                    reason: None,
                },
                reported: Some(Outcome::Done),
                limit_wait: Some(LimitWait {
                    waited: Duration::from_secs(100),
                    resumed_at: at(200),
                }),
                limit_warning: None,
                usage: crate::Usage::default(),
                used_model: None,
                routed: None,
            },
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(105),
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            clock(205).0,
        )
        .unwrap();

        // Long after the wait is over and the task is done, the step — and so the attempt's
        // own flat line — still carries what it waited for and when it resumed: not only
        // while the countdown above was still live.
        let entries = status(&journal, &clock(400), &no_run()).unwrap();
        let expected = Some(LimitWait {
            waited: Duration::from_secs(100),
            resumed_at: at(200),
        });
        assert_eq!(
            entries[0].attempt.outcome,
            AttemptOutcome::Reported(Outcome::Done)
        );
        assert_eq!(entries[0].attempt.limit_wait, expected);
        assert_eq!(entries[0].attempt.steps[0].limit_wait, expected);
        // The wait is folded into the step's own recorded time, not shown on the side.
        assert_eq!(entries[0].attempt.time_spent, Duration::from_secs(105));
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
        assert_eq!(attempt.outcome, AttemptOutcome::Reported(Outcome::TooLarge));
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
        assert_eq!(
            entries[0].attempt.outcome,
            AttemptOutcome::Reported(Outcome::NeedsInput)
        );
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
    fn a_task_marked_done_by_the_user_shows_done_with_the_reason_and_when() {
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
        crate::done_task(&journal, &clock(2), TaskId(1), "fixed by hand").unwrap();

        let entries = status(&journal, &clock(10), &no_run()).unwrap();
        assert_eq!(entries[0].status, TaskStatus::Done);
        assert_eq!(
            entries[0].done_by_user,
            Some(DoneMark {
                reason: "fixed by hand".to_owned(),
                at: clock(2).0,
            })
        );
        // The attempt itself still shows what actually happened to it — the manual marking is
        // recorded alongside it, not in place of it.
        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Unreported);
        assert_eq!(entries[0].attempt.reason.as_deref(), Some("it broke"));
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
        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Unreported);
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
    fn outcomes_remain_typed_facts() {
        assert_ne!(
            AttemptOutcome::Running,
            AttemptOutcome::Reported(Outcome::Done)
        );
        assert_ne!(AttemptOutcome::Unreported, AttemptOutcome::Interrupted);
        assert_ne!(AttemptOutcome::Passed, AttemptOutcome::Failed);
    }

    #[test]
    fn a_command_kind_step_that_did_not_pass_is_shown_failed_with_its_reason() {
        let end = AttemptEnd {
            duration: Duration::from_secs(1),
            status: TaskStatus::Failed,
            reason: Some("git identity is not configured".to_owned()),
            reported: None,
            limit_wait: None,
            limit_warning: None,
            usage: crate::Usage::default(),
            used_model: None,
            routed: None,
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
        crate::attempt::begin_step(
            &journal,
            &clock(100),
            TaskId(1),
            1,
            HEALTH_CHECK_STEP,
            None,
            None,
        )
        .unwrap();
        crate::attempt::end_step(
            &journal,
            &clock(104),
            TaskId(1),
            1,
            HEALTH_CHECK_STEP,
            StepEnd {
                run: AttemptRun {
                    duration: Duration::from_secs(4),
                    exit_code: Some(0),
                    status: TaskStatus::Done,
                    reason: None,
                },
                reported: None,
                limit_wait: None,
                limit_warning: None,
                usage: crate::Usage::default(),
                used_model: None,
                routed: None,
            },
        )
        .unwrap();
        crate::attempt::begin_step(
            &journal,
            &clock(104),
            TaskId(1),
            1,
            IMPLEMENTATION,
            None,
            None,
        )
        .unwrap();

        let entries = status(&journal, &clock(110), &a_live_run()).unwrap();
        assert_eq!(
            entries[0].attempt.steps,
            vec![
                StepLine {
                    step: HEALTH_CHECK_STEP.to_owned(),
                    // The health check is run by the tool itself, not the agent: it names no
                    // provider, even though the attempt ran with `echo`.
                    provider: None,
                    model: None,
                    session: None,
                    time_spent: Duration::from_secs(4),
                    outcome: AttemptOutcome::Passed,
                    reason: None,
                    findings: Vec::new(),
                    waiting: None,
                    limit_wait: None,
                    limit_warning: None,
                    routed: None,
                    more_time: None,
                    usage: crate::Usage::default(),
                },
                StepLine {
                    step: IMPLEMENTATION.to_owned(),
                    provider: Some("echo".to_owned()),
                    model: None,
                    session: None,
                    time_spent: Duration::from_secs(6),
                    outcome: AttemptOutcome::Running,
                    reason: None,
                    findings: Vec::new(),
                    waiting: None,
                    limit_wait: None,
                    limit_warning: None,
                    routed: None,
                    more_time: None,
                    usage: crate::Usage::default(),
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
                model: None,
                session: None,
                time_spent: Duration::ZERO,
                outcome: AttemptOutcome::Failed,
                reason: Some("uncommitted changes; commit or stash".to_owned()),
                findings: Vec::new(),
                waiting: None,
                limit_wait: None,
                limit_warning: None,
                routed: None,
                more_time: None,
                usage: crate::Usage::default(),
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

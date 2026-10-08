//! The run band: whether a run is going right now, where it most recently stopped and why, or
//! that the queue is idle — the one fact the queue screen and `status` show, under their own
//! summary, so an operator never has to go looking in a terminal that may be gone.

use std::time::{Duration, SystemTime};

use crate::status::{AttemptOutcome, OutputActivity, Wait, status_with_output};
use crate::{
    AttemptOutput, Clock, Journal, JournalError, NoAttemptOutput, Routed, RunLock, TaskId,
    TaskKind, TaskStatus, list_tasks,
};

/// Why a run is currently stopped at a task, read fresh from the journal each time — never
/// carried over from a process that is no longer running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopKind {
    /// The task's own attempt ended at `status` — `failed`, `blocked` or `failed-unknown` —
    /// with the reason and the router's last verdict its own ending recorded, when it has
    /// either.
    Ended {
        /// What the attempt ended at.
        status: TaskStatus,
        /// Why, when the ending has one.
        reason: Option<String>,
        /// The router's last verdict for the step that ended it, when it gave one.
        routed: Option<Routed>,
    },
    /// The sync, health-check or instructions gate ahead of the task's attempt refused to let
    /// it begin.
    EnvironmentFault {
        /// The gate's own step name.
        step: String,
        /// What it refused with, in the run's own words.
        reason: String,
    },
    /// The task is kind `human`: the run stops here without attempting it.
    HumanTask,
    /// The journal still calls the task running, but no run is alive to finish it: an earlier
    /// run was killed outright, and the next one has not yet reconciled it.
    Interrupted,
}

/// A run stopped here, at `task`, for `kind` — `at` when the journal recorded a timestamp for
/// it, `None` for a cause that is read fresh from the queue's own shape rather than from one
/// moment it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedBand {
    /// The task the run stopped at.
    pub task: TaskId,
    /// When it stopped, when the journal recorded a timestamp for it.
    pub at: Option<SystemTime>,
    /// Why.
    pub kind: StopKind,
}

/// An attempt running right now, with the same facts the status screen's own attempt line
/// carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningBand {
    /// The task being attempted.
    pub task: TaskId,
    /// The step currently running.
    pub step: String,
    /// The provider running it, when an agent runs it.
    pub provider: Option<String>,
    /// The model it runs with, when it has one.
    pub model: Option<String>,
    /// How long the current step has run so far.
    pub time_spent: Duration,
    /// What the current step is waiting for, and how long remains, while it waits for its
    /// provider's usage limit.
    pub waiting: Option<Wait>,
    /// Live provider-output facts, when they are available.
    pub output_activity: Option<OutputActivity>,
}

/// The queue's run band: one of three states, read fresh from the journal every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunBand {
    /// An attempt is running right now.
    Running(RunningBand),
    /// The run most recently stopped here, and nothing has continued past it since.
    Stopped(StoppedBand),
    /// Nothing is running and nothing is stopped: `pending` tasks are ready for `r`, or none
    /// are left.
    Idle {
        /// How many tasks are pending right now.
        pending: usize,
    },
}

/// Reads the queue's current run band.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when `lock` cannot be used.
pub fn run_band(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
) -> Result<RunBand, JournalError> {
    run_band_with_output(journal, clock, lock, &NoAttemptOutput, Duration::MAX)
}

/// Reads the queue's current run band, with the live output facts for a running attempt.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when `lock` cannot be used.
pub fn run_band_with_output(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: &impl AttemptOutput,
    silent_after: Duration,
) -> Result<RunBand, JournalError> {
    let tasks = list_tasks(journal)?;
    let pending = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Pending)
        .count();
    let Some(next) = tasks
        .into_iter()
        .find(|task| task.status != TaskStatus::Done)
    else {
        return Ok(RunBand::Idle { pending: 0 });
    };
    if next.status == TaskStatus::Pending {
        return pending_band(journal, &next, pending);
    }
    attempted_band(journal, clock, lock, output, silent_after, &next)
}

/// The band for `next`, the head of the queue, while it is still `pending`: stopped at it
/// being kind `human`, or at a gate that refused its attempt, or idle when neither applies.
fn pending_band(
    journal: &impl Journal,
    next: &crate::Task,
    pending: usize,
) -> Result<RunBand, JournalError> {
    if next.kind == TaskKind::Human {
        return Ok(stopped(next.id, None, StopKind::HumanTask));
    }
    Ok(match crate::attempt::gate_stop_of(journal, next.id)? {
        Some((step, reason, at)) => stopped(
            next.id,
            Some(at),
            StopKind::EnvironmentFault { step, reason },
        ),
        None => RunBand::Idle { pending },
    })
}

/// The band for `next`, the head of the queue, once it has been attempted at least once:
/// running while its attempt still is, stopped at how it ended otherwise.
fn attempted_band(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: &impl AttemptOutput,
    silent_after: Duration,
    next: &crate::Task,
) -> Result<RunBand, JournalError> {
    let entries = status_with_output(journal, clock, lock, output, silent_after)?;
    let Some(entry) = entries.into_iter().find(|entry| entry.task == next.id) else {
        unreachable!("a task running or already ended was attempted, so status has its entry");
    };
    Ok(match entry.attempt.outcome {
        AttemptOutcome::Running | AttemptOutcome::Waiting => RunBand::Running(RunningBand {
            task: next.id,
            step: entry.attempt.step,
            provider: entry.attempt.provider,
            model: entry.attempt.model,
            time_spent: entry.attempt.time_spent,
            waiting: entry.attempt.waiting,
            output_activity: entry.attempt.output_activity,
        }),
        AttemptOutcome::Interrupted => stopped(next.id, None, StopKind::Interrupted),
        _ => {
            let at = crate::attempt::ended_at(journal, next.id, entry.attempt.number)?;
            stopped(
                next.id,
                at,
                StopKind::Ended {
                    status: next.status,
                    reason: entry.attempt.reason,
                    routed: entry.attempt.routed,
                },
            )
        }
    })
}

fn stopped(task: TaskId, at: Option<SystemTime>, kind: StopKind) -> RunBand {
    RunBand::Stopped(StoppedBand { task, at, kind })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::fakes::{FakeClock, FakeJournal, FakeRunLock, at, draft};
    use crate::{
        AttemptRun, Event, HEALTH_CHECK_STEP, Outcome, Placement, TaskDraft, TaskId, TaskKind,
        TaskStatus, add_task, report,
    };

    use super::*;

    fn clock(seconds: u64) -> FakeClock {
        FakeClock(at(seconds))
    }

    fn no_run() -> FakeRunLock {
        FakeRunLock::free()
    }

    fn a_live_run() -> FakeRunLock {
        FakeRunLock::held_by(Some(4_321))
    }

    #[test]
    fn an_empty_queue_is_idle_with_nothing_pending() {
        let journal = FakeJournal::default();
        assert_eq!(
            run_band(&journal, &clock(0), &no_run()).unwrap(),
            RunBand::Idle { pending: 0 }
        );
    }

    #[test]
    fn pending_tasks_never_attempted_are_idle_with_their_count() {
        let journal = FakeJournal::default();
        for title in ["a", "b"] {
            add_task(&journal, &clock(0), &draft(title), Placement::End).unwrap();
        }
        assert_eq!(
            run_band(&journal, &clock(0), &no_run()).unwrap(),
            RunBand::Idle { pending: 2 }
        );
    }

    #[test]
    fn every_task_done_is_idle_with_nothing_pending() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        let number = crate::attempt::begin_attempt(&journal, &clock(0), TaskId(1)).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            number,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            at(1),
        )
        .unwrap();
        assert_eq!(
            run_band(&journal, &clock(1), &no_run()).unwrap(),
            RunBand::Idle { pending: 0 }
        );
    }

    #[test]
    fn a_running_attempt_shows_a_running_band_with_its_current_step() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();

        let band = run_band(&journal, &clock(30), &a_live_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Running(RunningBand {
                task: TaskId(1),
                step: crate::IMPLEMENTATION.to_owned(),
                provider: Some("echo".to_owned()),
                model: None,
                time_spent: Duration::from_secs(30),
                waiting: None,
                output_activity: Some(OutputActivity {
                    last_output_at: None,
                    silent_for: Duration::from_secs(30),
                    active: false,
                    may_be_stuck: false,
                }),
            })
        );
    }

    #[test]
    fn an_attempt_waiting_on_a_usage_limit_is_still_a_running_band_carrying_the_countdown() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        crate::attempt::begin_step(
            &journal,
            &clock(0),
            TaskId(1),
            1,
            crate::IMPLEMENTATION,
            None,
            None,
        )
        .unwrap();
        crate::attempt::record_waiting(
            &journal,
            &clock(0),
            TaskId(1),
            1,
            crate::IMPLEMENTATION,
            at(100),
            crate::WaitReason::UsageLimit,
        )
        .unwrap();

        let band = run_band(&journal, &clock(30), &a_live_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Running(RunningBand {
                task: TaskId(1),
                step: crate::IMPLEMENTATION.to_owned(),
                provider: Some("echo".to_owned()),
                model: None,
                time_spent: Duration::from_secs(30),
                waiting: Some(Wait {
                    reason: crate::WaitReason::UsageLimit,
                    remaining: Duration::from_secs(70),
                }),
                output_activity: None,
            })
        );
    }

    #[test]
    fn a_task_left_running_with_no_run_alive_is_a_stopped_band_interrupted_with_no_time() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();

        let band = run_band(&journal, &clock(30), &no_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Stopped(StoppedBand {
                task: TaskId(1),
                at: None,
                kind: StopKind::Interrupted,
            })
        );
    }

    #[test]
    fn a_failed_task_is_a_stopped_band_naming_its_reason_and_when() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        report(
            &journal,
            &clock(10),
            &crate::AttemptToken::new("proj", TaskId(1), 1),
            Outcome::Failed,
            Some("it broke"),
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::from_secs(12),
                exit_code: Some(1),
                status: TaskStatus::Failed,
                reason: Some("it broke"),
            },
            at(12),
        )
        .unwrap();

        let band = run_band(&journal, &clock(100), &no_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Stopped(StoppedBand {
                task: TaskId(1),
                at: Some(at(12)),
                kind: StopKind::Ended {
                    status: TaskStatus::Failed,
                    reason: Some("it broke".to_owned()),
                    routed: None,
                },
            })
        );
    }

    #[test]
    fn a_blocked_task_is_a_stopped_band_too() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        report(
            &journal,
            &clock(5),
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
            at(5),
        )
        .unwrap();

        let band = run_band(&journal, &clock(100), &no_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Stopped(StoppedBand {
                task: TaskId(1),
                at: Some(at(5)),
                kind: StopKind::Ended {
                    status: TaskStatus::Blocked,
                    reason: Some("which path?".to_owned()),
                    routed: None,
                },
            })
        );
    }

    #[test]
    fn a_human_task_at_the_head_of_the_queue_is_a_stopped_band_naming_it() {
        let journal = FakeJournal::default();
        add_task(
            &journal,
            &clock(0),
            &TaskDraft {
                kind: TaskKind::Human,
                ..draft("a")
            },
            Placement::End,
        )
        .unwrap();

        let band = run_band(&journal, &clock(0), &no_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Stopped(StoppedBand {
                task: TaskId(1),
                at: None,
                kind: StopKind::HumanTask,
            })
        );
    }

    #[test]
    fn a_gate_stop_is_a_stopped_band_naming_the_gate_and_when() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        let read = journal.events().unwrap().len();
        journal
            .append_events(
                &[Event::GateFailed {
                    id: TaskId(1),
                    step: HEALTH_CHECK_STEP.to_owned(),
                    reason: "exited with code 1".to_owned(),
                    at: at(3),
                }],
                read,
            )
            .unwrap();

        let band = run_band(&journal, &clock(10), &no_run()).unwrap();
        assert_eq!(
            band,
            RunBand::Stopped(StoppedBand {
                task: TaskId(1),
                at: Some(at(3)),
                kind: StopKind::EnvironmentFault {
                    step: HEALTH_CHECK_STEP.to_owned(),
                    reason: "exited with code 1".to_owned(),
                },
            })
        );
    }

    #[test]
    fn a_failed_task_still_blocks_the_band_even_with_later_pending_tasks() {
        let journal = FakeJournal::default();
        for title in ["a", "b"] {
            add_task(&journal, &clock(0), &draft(title), Placement::End).unwrap();
        }
        crate::attempt::begin_attempt_running(&journal, &clock(0), TaskId(1), "echo", None)
            .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(1),
                status: TaskStatus::Failed,
                reason: Some("it broke"),
            },
            at(1),
        )
        .unwrap();

        let band = run_band(&journal, &clock(10), &no_run()).unwrap();
        assert!(matches!(
            band,
            RunBand::Stopped(StoppedBand {
                task: TaskId(1),
                ..
            })
        ));
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(run_band(&journal, &clock(0), &no_run()), Err(failure));
    }
}

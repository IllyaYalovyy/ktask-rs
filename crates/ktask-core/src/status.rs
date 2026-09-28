//! `status`: what ran and how it ended, for every task that has been attempted.

use std::fmt;
use std::time::Duration;

use crate::{Clock, Journal, JournalError, Outcome, Task, TaskId, TaskStatus, list_all_tasks};

/// The one step kind that exists so far: an agent implementing the task.
pub const IMPLEMENTATION: &str = "implementation";

/// How an attempt's outcome is labelled: as the agent itself reported it, or as the tool
/// observed it when the agent never reported at all — a crash, a kill past the time limit, or
/// a run left running by a killed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Still running: there is no outcome yet.
    Running,
    /// The agent's own reported outcome.
    Reported(Outcome),
    /// The tool observed the attempt end with no report from the agent.
    Unreported,
}

impl AttemptOutcome {
    /// The word the outcome is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Reported(outcome) => outcome.as_str(),
            Self::Unreported => TaskStatus::FailedUnknown.as_str(),
        }
    }
}

impl fmt::Display for AttemptOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One task's attempt, as `status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLine {
    /// The attempt's number.
    pub number: u32,
    /// The step it ran: always [`IMPLEMENTATION`], the only step kind that exists so far.
    pub step: &'static str,
    /// The provider it ran with, once that is known.
    pub provider: Option<String>,
    /// How long it has run: the recorded duration once it has ended, elapsed time so far
    /// while it is running.
    pub time_spent: Duration,
    /// What it ended at, or that it is still running.
    pub outcome: AttemptOutcome,
    /// Why, when the outcome is not a success.
    pub reason: Option<String>,
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
}

/// The [`StatusEntry`] for `task`, given its most recent attempt and the agent's own report of
/// it, when there was one.
fn entry_for(
    task: Task,
    attempt: crate::Attempt,
    reported: Option<(Outcome, Option<String>)>,
    clock: &impl Clock,
) -> StatusEntry {
    let (outcome, reason, time_spent) = match (&attempt.ended, reported) {
        (Some(end), Some((outcome, reason))) => {
            (AttemptOutcome::Reported(outcome), reason, end.duration)
        }
        (Some(end), None) => (AttemptOutcome::Unreported, end.reason.clone(), end.duration),
        (None, _) => (
            AttemptOutcome::Running,
            None,
            clock
                .now()
                .duration_since(attempt.started_at)
                .unwrap_or_default(),
        ),
    };
    StatusEntry {
        task: task.id,
        title: task.title,
        status: task.status,
        attempt: AttemptLine {
            number: attempt.number,
            step: IMPLEMENTATION,
            provider: attempt.provider,
            time_spent,
            outcome,
            reason,
        },
    }
}

/// Use case: what ran and how it ended, in queue order — one [`StatusEntry`] for every task
/// that was attempted at least once, cancelled tasks included when they were. A task never
/// attempted, pending or cancelled before it ever ran, is left out.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn status(
    journal: &impl Journal,
    clock: &impl Clock,
) -> Result<Vec<StatusEntry>, JournalError> {
    let mut entries = Vec::new();
    for task in list_all_tasks(journal)? {
        let Some(attempt) = journal.last_attempt(task.id)? else {
            continue;
        };
        let reported = journal.last_report(task.id, attempt.number)?;
        entries.push(entry_for(task, attempt, reported, clock));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{
        AttemptRun, Outcome, Placement, TaskId, TaskStatus, add_task, report, start_attempt,
    };

    use super::*;

    fn clock(seconds: u64) -> FakeClock {
        FakeClock(at(seconds))
    }

    #[test]
    fn a_pending_task_is_left_out() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        assert_eq!(status(&journal, &clock(0)).unwrap(), vec![]);
    }

    #[test]
    fn a_cancelled_task_that_was_never_attempted_is_left_out() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        crate::remove_task(&journal, &clock(0), TaskId(1)).unwrap();
        assert_eq!(status(&journal, &clock(0)).unwrap(), vec![]);
    }

    #[test]
    fn an_empty_queue_has_no_status_and_a_project_with_no_attempts_reports_nothing() {
        assert_eq!(status(&FakeJournal::default(), &clock(0)).unwrap(), vec![]);
    }

    /// A journal with one task titled `a`, whose one attempt was started at second 100 and ran
    /// with `echo`.
    fn journal_with_a_started_attempt() -> FakeJournal {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(0), &draft("a"), Placement::End).unwrap();
        start_attempt(&journal, &clock(100), "proj", TaskId(1)).unwrap();
        journal
            .attempt_running(TaskId(1), 1, "echo", clock(100).0)
            .unwrap();
        journal
    }

    #[test]
    fn a_running_attempt_shows_no_outcome_and_its_elapsed_time_so_far() {
        let journal = journal_with_a_started_attempt();
        let entries = status(&journal, &clock(130)).unwrap();
        assert_eq!(
            entries,
            vec![StatusEntry {
                task: TaskId(1),
                title: "a".to_owned(),
                status: TaskStatus::Running,
                attempt: AttemptLine {
                    number: 1,
                    step: IMPLEMENTATION,
                    provider: Some("echo".to_owned()),
                    time_spent: Duration::from_secs(30),
                    outcome: AttemptOutcome::Running,
                    reason: None,
                },
            }]
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
        journal
            .end_attempt(
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
        status(&journal, &clock(200)).unwrap().remove(0).attempt
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
        journal
            .end_attempt(
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
        let entries = status(&journal, &clock(200)).unwrap();
        assert_eq!(entries[0].status, TaskStatus::Blocked);
        assert_eq!(
            entries[0].attempt.outcome,
            AttemptOutcome::Reported(Outcome::NeedsInput)
        );
        assert_eq!(entries[0].attempt.outcome.as_str(), "needs-input");
        assert_eq!(entries[0].attempt.reason.as_deref(), Some("which path?"));
    }

    #[test]
    fn no_report_at_all_shows_the_tools_own_failed_unknown_outcome_and_reason() {
        let journal = journal_with_a_started_attempt();
        journal
            .end_attempt(
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
        let entries = status(&journal, &clock(200)).unwrap();
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
    fn entries_are_in_queue_order_and_pending_tasks_are_skipped_in_between() {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(0), &draft(title), Placement::End).unwrap();
        }
        start_attempt(&journal, &clock(0), "proj", TaskId(1)).unwrap();
        journal
            .attempt_running(TaskId(1), 1, "echo", at(0))
            .unwrap();
        report(
            &journal,
            &clock(1),
            &crate::AttemptToken::new("proj", TaskId(1), 1),
            Outcome::Done,
            None,
        )
        .unwrap();
        journal
            .end_attempt(
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
        start_attempt(&journal, &clock(0), "proj", TaskId(3)).unwrap();
        journal
            .attempt_running(TaskId(3), 1, "echo", at(0))
            .unwrap();
        report(
            &journal,
            &clock(1),
            &crate::AttemptToken::new("proj", TaskId(3), 1),
            Outcome::Failed,
            Some("nope"),
        )
        .unwrap();
        journal
            .end_attempt(
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

        let entries = status(&journal, &clock(2)).unwrap();
        let ids: Vec<_> = entries.iter().map(|entry| entry.task).collect();
        assert_eq!(ids, [TaskId(1), TaskId(3)]);
        assert_eq!(entries[0].status, TaskStatus::Done);
        assert_eq!(entries[1].status, TaskStatus::Failed);
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(status(&journal, &clock(0)), Err(failure));
    }

    #[test]
    fn outcome_labels_read_as_expected() {
        assert_eq!(AttemptOutcome::Running.as_str(), "running");
        assert_eq!(AttemptOutcome::Reported(Outcome::Done).as_str(), "done");
        assert_eq!(AttemptOutcome::Unreported.as_str(), "failed-unknown");
        assert_eq!(AttemptOutcome::Running.to_string(), "running");
    }
}

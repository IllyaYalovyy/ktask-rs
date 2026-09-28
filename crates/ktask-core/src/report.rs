//! An agent stating the outcome of an attempt.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::{Clock, Journal, RecordReportError, TaskId};

/// What an attempt ended with, as the agent that ran it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The task is done.
    Done,
    /// The attempt failed.
    Failed,
    /// The agent needs a decision from the operator before it can continue.
    NeedsInput,
    /// The task is too big to do in one attempt.
    TooLarge,
}

impl Outcome {
    /// The name the outcome is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::NeedsInput => "needs-input",
            Self::TooLarge => "too-large",
        }
    }

    /// Whether this outcome must be reported with a reason.
    #[must_use]
    pub fn needs_reason(self) -> bool {
        !matches!(self, Self::Done)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Outcome {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [Self::Done, Self::Failed, Self::NeedsInput, Self::TooLarge]
            .into_iter()
            .find(|outcome| outcome.as_str() == text)
            .ok_or_else(|| {
                format!("unknown outcome {text:?}: expected done, failed, needs-input or too-large")
            })
    }
}

/// The token an attempt is reported with: names the project, task and attempt it belongs to,
/// so `ktask-rs report` needs no `--project` and works from any directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptToken {
    /// The project the attempt belongs to.
    pub project: String,
    /// The task the attempt belongs to.
    pub task: TaskId,
    /// The attempt's number.
    pub number: u32,
}

impl AttemptToken {
    /// The token for attempt `number` of `task` in `project`.
    #[must_use]
    pub fn new(project: impl Into<String>, task: TaskId, number: u32) -> Self {
        Self {
            project: project.into(),
            task,
            number,
        }
    }
}

impl fmt::Display for AttemptToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.project, self.task, self.number)
    }
}

impl FromStr for AttemptToken {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let malformed = || format!("malformed token {text:?}: expected PROJECT/TASK/ATTEMPT");
        let mut parts = text.split('/');
        let (Some(project), Some(task), Some(number), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed());
        };
        if project.is_empty() {
            return Err(malformed());
        }
        let task = task.parse::<u64>().map_err(|_| malformed())?;
        let number = number.parse::<u32>().map_err(|_| malformed())?;
        Ok(Self {
            project: project.to_owned(),
            task: TaskId(task),
            number,
        })
    }
}

/// Why a report was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// `outcome` needs a reason, and none, or only a blank one, was given.
    ReasonRequired(Outcome),
    /// The attempt the token names is unknown or has ended.
    Record(RecordReportError),
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReasonRequired(outcome) => {
                write!(f, "outcome {outcome} needs a reason: pass --reason")
            }
            Self::Record(error) => error.fmt(f),
        }
    }
}

impl Error for ReportError {}

impl From<RecordReportError> for ReportError {
    fn from(error: RecordReportError) -> Self {
        Self::Record(error)
    }
}

/// Use case: starts the next attempt at the pending task numbered `id` in `project`, marking
/// the task running, and returns the token that names it.
///
/// # Errors
///
/// Fails, changing nothing, when there is no such task, when it is not pending, or when the
/// journal cannot be written.
pub fn start_attempt(
    journal: &impl Journal,
    clock: &impl Clock,
    project: &str,
    id: TaskId,
) -> Result<AttemptToken, crate::BeginAttemptError> {
    let number = crate::attempt::begin_attempt(journal, clock, id)?;
    Ok(AttemptToken::new(project, id, number))
}

/// Use case: records `outcome` (and `reason`, when the outcome needs one) for the attempt
/// `token` names, as one event in the journal `token`'s project belongs to.
///
/// # Errors
///
/// Fails, recording nothing, when `outcome` needs a reason and none, or only a blank one, was
/// given, or when the journal reports the attempt is unknown or has ended.
pub fn report(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    outcome: Outcome,
    reason: Option<&str>,
) -> Result<(), ReportError> {
    let blank = reason.is_none_or(|reason| reason.trim().is_empty());
    if outcome.needs_reason() && blank {
        return Err(ReportError::ReasonRequired(outcome));
    }
    crate::attempt::record_report(journal, clock, token.task, token.number, outcome, reason)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{BeginAttemptError, Placement, TaskId, TaskStatus, add_task};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(1_000))
    }

    fn journal_with_a_pending_task() -> FakeJournal {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        journal
    }

    #[test]
    fn starting_an_attempt_numbers_it_from_one_and_marks_the_task_running() {
        let journal = journal_with_a_pending_task();
        let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        assert_eq!(token, AttemptToken::new("proj", TaskId(1), 1));
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Running
        );
    }

    #[test]
    fn a_token_reads_back_from_its_display_form() {
        let token = AttemptToken::new("proj", TaskId(7), 3);
        assert_eq!(token.to_string(), "proj/7/3");
        assert_eq!(token.to_string().parse(), Ok(token));
    }

    #[test]
    fn a_malformed_token_names_the_problem() {
        for text in [
            "",
            "proj",
            "proj/7",
            "proj/7/3/extra",
            "/7/3",
            "proj/x/3",
            "proj/7/x",
        ] {
            assert!(
                text.parse::<AttemptToken>()
                    .unwrap_err()
                    .contains("malformed token"),
                "{text}"
            );
        }
    }

    #[test]
    fn starting_an_attempt_twice_is_refused_the_second_time() {
        let journal = journal_with_a_pending_task();
        start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        assert_eq!(
            start_attempt(&journal, &clock(), "proj", TaskId(1)),
            Err(BeginAttemptError::NotPending(TaskId(1)))
        );
    }

    #[test]
    fn starting_an_attempt_at_an_unknown_task_is_refused() {
        let journal = FakeJournal::default();
        assert_eq!(
            start_attempt(&journal, &clock(), "proj", TaskId(9)),
            Err(BeginAttemptError::UnknownTask(TaskId(9)))
        );
    }

    #[test]
    fn a_valid_report_for_a_running_attempt_is_recorded() {
        let journal = journal_with_a_pending_task();
        let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Done, None),
            Ok(())
        );
    }

    #[test]
    fn done_needs_no_reason_but_every_other_outcome_does() {
        for outcome in [Outcome::Failed, Outcome::NeedsInput, Outcome::TooLarge] {
            let journal = journal_with_a_pending_task();
            let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
            for blank in [None, Some(""), Some("   ")] {
                assert_eq!(
                    report(&journal, &clock(), &token, outcome, blank),
                    Err(ReportError::ReasonRequired(outcome)),
                    "{outcome} with {blank:?}"
                );
            }
            assert_eq!(
                report(&journal, &clock(), &token, outcome, Some("because")),
                Ok(())
            );
        }
        let journal = journal_with_a_pending_task();
        let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Done, None),
            Ok(())
        );
    }

    #[test]
    fn a_second_valid_report_for_the_same_attempt_is_recorded_too() {
        let journal = journal_with_a_pending_task();
        let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        report(&journal, &clock(), &token, Outcome::Failed, Some("first")).unwrap();
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Done, None),
            Ok(())
        );
    }

    #[test]
    fn an_unknown_task_in_the_token_is_refused_and_named() {
        let journal = FakeJournal::default();
        let token = AttemptToken::new("proj", TaskId(9), 1);
        let error = report(&journal, &clock(), &token, Outcome::Done, None).unwrap_err();
        assert_eq!(
            error,
            ReportError::Record(RecordReportError::UnknownAttempt {
                task: TaskId(9),
                number: 1
            })
        );
        assert!(error.to_string().contains("no attempt 1 of task 9"));
    }

    #[test]
    fn a_wrong_attempt_number_is_refused_as_unknown() {
        let journal = journal_with_a_pending_task();
        let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        let wrong = AttemptToken::new(token.project, token.task, token.number + 1);
        assert_eq!(
            report(&journal, &clock(), &wrong, Outcome::Done, None),
            Err(ReportError::Record(RecordReportError::UnknownAttempt {
                task: TaskId(1),
                number: 2
            }))
        );
    }

    #[test]
    fn an_ended_attempt_is_refused_and_named() {
        let journal = journal_with_a_pending_task();
        let token = start_attempt(&journal, &clock(), "proj", TaskId(1)).unwrap();
        crate::remove_task(&journal, &clock(), TaskId(1)).unwrap();
        let error = report(&journal, &clock(), &token, Outcome::Done, None).unwrap_err();
        assert_eq!(
            error,
            ReportError::Record(RecordReportError::AttemptEnded {
                task: TaskId(1),
                number: 1
            })
        );
        assert!(error.to_string().contains("attempt 1 of task 1 has ended"));
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = crate::JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        assert_eq!(
            start_attempt(&journal, &clock(), "proj", TaskId(1)),
            Err(BeginAttemptError::Journal(failure.clone()))
        );
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Done, None),
            Err(ReportError::Record(RecordReportError::Journal(failure)))
        );
    }
}

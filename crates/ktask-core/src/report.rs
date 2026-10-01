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
    /// The reviewer accepted the task's implementation.
    Approved,
    /// The reviewer found something to fix: its findings are the reason.
    ChangesRequested,
    /// The tester accepted the task's implementation.
    Accepted,
    /// The tester found something that failed: what failed is the reason.
    Rejected,
    /// The resolver decided a fresh attempt at the implementation step is worth trying.
    Retry,
    /// The resolver decided the task should end `failed`: the reason is why.
    Stop,
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
            Self::Approved => "approved",
            Self::ChangesRequested => "changes-requested",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Retry => "retry",
            Self::Stop => "stop",
        }
    }

    /// Whether this outcome must be reported with a reason.
    #[must_use]
    pub fn needs_reason(self) -> bool {
        !matches!(
            self,
            Self::Done | Self::Approved | Self::Accepted | Self::Retry
        )
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
        [
            Self::Done,
            Self::Failed,
            Self::NeedsInput,
            Self::TooLarge,
            Self::Approved,
            Self::ChangesRequested,
            Self::Accepted,
            Self::Rejected,
            Self::Retry,
            Self::Stop,
        ]
        .into_iter()
        .find(|outcome| outcome.as_str() == text)
        .ok_or_else(|| {
            format!(
                "unknown outcome {text:?}: expected done, failed, needs-input, too-large, \
                 approved, changes-requested, accepted, rejected, retry or stop"
            )
        })
    }
}

/// The outcomes that belong to step `step`, in the order they should be named when one that
/// does not belong is refused. `None` when `step` is not one an agent reports an outcome for
/// itself — the sync and health-check steps, which the tool records as already having passed —
/// so any outcome is accepted rather than refused against an empty list.
#[must_use]
fn outcomes_for_step(step: &str) -> Option<&'static [Outcome]> {
    if step == crate::IMPLEMENTATION {
        Some(&[
            Outcome::Done,
            Outcome::Failed,
            Outcome::NeedsInput,
            Outcome::TooLarge,
        ])
    } else if step == crate::REVIEW_STEP {
        Some(&[Outcome::Approved, Outcome::ChangesRequested])
    } else if step == crate::TEST_STEP {
        Some(&[Outcome::Accepted, Outcome::Rejected])
    } else if step == crate::RESOLVE_STEP {
        Some(&[Outcome::Retry, Outcome::Stop])
    } else {
        None
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
    /// `outcome` does not belong to the step that is currently running: `step` is the one
    /// running, `expected` names every outcome that does belong to it.
    WrongStep {
        /// The outcome that was refused.
        outcome: Outcome,
        /// The step that is running.
        step: String,
        /// The outcomes that do belong to `step`.
        expected: Vec<Outcome>,
    },
    /// The attempt the token names is unknown or has ended.
    Record(RecordReportError),
    /// `--same-session` was given, but the provider configured for this project does not
    /// support resuming a session at all.
    ResumeNotSupported,
    /// `--same-session` was given, but the attempt the token names reported no session to
    /// continue.
    NoSessionRecorded,
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReasonRequired(outcome) => {
                write!(f, "outcome {outcome} needs a reason: pass --reason")
            }
            Self::WrongStep {
                outcome,
                step,
                expected,
            } => {
                let expected = expected
                    .iter()
                    .map(|outcome| outcome.as_str())
                    .collect::<Vec<_>>()
                    .join(" or ");
                write!(
                    f,
                    "outcome {outcome} does not belong to the {step} step: expected {expected}"
                )
            }
            Self::Record(error) => error.fmt(f),
            Self::ResumeNotSupported => f.write_str(
                "the provider configured for this project does not support resuming a \
                 session: --same-session cannot be used",
            ),
            Self::NoSessionRecorded => f.write_str(
                "this attempt reported no session to continue: --same-session needs one",
            ),
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
/// given; when `outcome` does not belong to the step currently running for this attempt; or
/// when the journal reports the attempt is unknown or has ended.
pub fn report(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    outcome: Outcome,
    reason: Option<&str>,
) -> Result<(), ReportError> {
    report_impl(journal, clock, token, outcome, reason, None, false, false)
}

/// Use case: records the resolver's `retry` decision for the attempt `token` names, carrying
/// `model` — the model it named for the task's next attempt, when it named one — the same as
/// [`report`] with [`Outcome::Retry`], but with nowhere else for `model` to be given.
/// `same_session` asks the task's next attempt to resume this attempt's own session instead of
/// starting fresh; `provider_supports_resume` is whatever the project's configured provider
/// reports for itself. `reset_tree` asks the working tree to be returned to the commit this
/// attempt started from before the task's next attempt begins.
///
/// # Errors
///
/// Fails, recording nothing, when `retry` does not belong to the step currently running for
/// this attempt — only the resolve step accepts it; when the journal reports the attempt is
/// unknown or has ended; when `same_session` is asked for a provider that does not support
/// resuming at all ([`ReportError::ResumeNotSupported`]); or when `same_session` is asked and
/// this attempt reported no session to continue ([`ReportError::NoSessionRecorded`]).
#[allow(clippy::too_many_arguments)]
pub fn report_retry(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    model: Option<&str>,
    same_session: bool,
    provider_supports_resume: bool,
    reset_tree: bool,
) -> Result<(), ReportError> {
    if same_session {
        if !provider_supports_resume {
            return Err(ReportError::ResumeNotSupported);
        }
        let session = crate::attempt::last_session(journal, token.task, token.number)
            .map_err(RecordReportError::from)?;
        if session.is_none() {
            return Err(ReportError::NoSessionRecorded);
        }
    }
    report_impl(
        journal,
        clock,
        token,
        Outcome::Retry,
        None,
        model,
        same_session,
        reset_tree,
    )
}

/// [`report`] and [`report_retry`]'s shared work: both are this, differing only in whether
/// `retry_model` is ever anything but `None`, and `retry_same_session` and `retry_reset_tree`
/// ever `true`.
#[allow(clippy::too_many_arguments)]
fn report_impl(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    outcome: Outcome,
    reason: Option<&str>,
    retry_model: Option<&str>,
    retry_same_session: bool,
    retry_reset_tree: bool,
) -> Result<(), ReportError> {
    let blank = reason.is_none_or(|reason| reason.trim().is_empty());
    if outcome.needs_reason() && blank {
        return Err(ReportError::ReasonRequired(outcome));
    }
    if let Some(step) =
        crate::attempt::current_step(journal, token.task).map_err(RecordReportError::from)?
        && let Some(expected) = outcomes_for_step(&step)
        && !expected.contains(&outcome)
    {
        return Err(ReportError::WrongStep {
            outcome,
            step,
            expected: expected.to_vec(),
        });
    }
    crate::attempt::record_report(
        journal,
        clock,
        token.task,
        token.number,
        outcome,
        reason,
        retry_model,
        retry_same_session,
        retry_reset_tree,
    )?;
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
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            token.number,
            crate::journal::AttemptRun {
                duration: std::time::Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            clock().now(),
        )
        .unwrap();
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

    /// `journal_with_a_pending_task`, with task 1's attempt begun, running, and its step named
    /// `step` begun but not yet ended — the state `report` sees while an agent is mid-step.
    fn journal_with_a_running_step(step: &str) -> FakeJournal {
        let journal = journal_with_a_pending_task();
        crate::attempt::begin_attempt_running(&journal, &clock(), TaskId(1), "test", None).unwrap();
        crate::attempt::begin_step(&journal, &clock(), TaskId(1), 1, step, None).unwrap();
        journal
    }

    #[test]
    fn an_outcome_that_does_not_belong_to_the_running_step_is_refused_naming_the_ones_that_do() {
        let journal = journal_with_a_running_step(crate::REVIEW_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);

        let error = report(&journal, &clock(), &token, Outcome::Done, None).unwrap_err();
        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Done,
                step: crate::REVIEW_STEP.to_owned(),
                expected: vec![Outcome::Approved, Outcome::ChangesRequested],
            }
        );
        let message = error.to_string();
        assert!(
            message.contains("does not belong to the review step")
                && message.contains("approved")
                && message.contains("changes-requested"),
            "{message}"
        );
        // Nothing was recorded: the outcome that does belong still works afterwards.
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Approved, None),
            Ok(())
        );
    }

    #[test]
    fn a_review_only_outcome_is_refused_during_the_implementation_step_naming_its_own() {
        let journal = journal_with_a_running_step(crate::IMPLEMENTATION);
        let token = AttemptToken::new("proj", TaskId(1), 1);

        let error = report(&journal, &clock(), &token, Outcome::Approved, None).unwrap_err();
        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Approved,
                step: crate::IMPLEMENTATION.to_owned(),
                expected: vec![
                    Outcome::Done,
                    Outcome::Failed,
                    Outcome::NeedsInput,
                    Outcome::TooLarge
                ],
            }
        );
    }

    #[test]
    fn approved_needs_no_reason_but_changes_requested_does() {
        let journal = journal_with_a_running_step(crate::REVIEW_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        for blank in [None, Some(""), Some("   ")] {
            assert_eq!(
                report(&journal, &clock(), &token, Outcome::ChangesRequested, blank),
                Err(ReportError::ReasonRequired(Outcome::ChangesRequested))
            );
        }
        assert_eq!(
            report(
                &journal,
                &clock(),
                &token,
                Outcome::ChangesRequested,
                Some("fix this")
            ),
            Ok(())
        );
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Approved, None),
            Ok(())
        );
    }

    #[test]
    fn a_test_only_outcome_is_refused_during_the_review_step_naming_its_own() {
        let journal = journal_with_a_running_step(crate::REVIEW_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);

        let error = report(&journal, &clock(), &token, Outcome::Accepted, None).unwrap_err();
        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Accepted,
                step: crate::REVIEW_STEP.to_owned(),
                expected: vec![Outcome::Approved, Outcome::ChangesRequested],
            }
        );
    }

    #[test]
    fn a_review_only_outcome_is_refused_during_the_test_step_naming_its_own() {
        let journal = journal_with_a_running_step(crate::TEST_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);

        let error = report(&journal, &clock(), &token, Outcome::Approved, None).unwrap_err();
        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Approved,
                step: crate::TEST_STEP.to_owned(),
                expected: vec![Outcome::Accepted, Outcome::Rejected],
            }
        );
        let message = error.to_string();
        assert!(
            message.contains("does not belong to the testing step")
                && message.contains("accepted")
                && message.contains("rejected"),
            "{message}"
        );
    }

    #[test]
    fn accepted_needs_no_reason_but_rejected_does() {
        let journal = journal_with_a_running_step(crate::TEST_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        for blank in [None, Some(""), Some("   ")] {
            assert_eq!(
                report(&journal, &clock(), &token, Outcome::Rejected, blank),
                Err(ReportError::ReasonRequired(Outcome::Rejected))
            );
        }
        assert_eq!(
            report(
                &journal,
                &clock(),
                &token,
                Outcome::Rejected,
                Some("crashes on startup")
            ),
            Ok(())
        );
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Accepted, None),
            Ok(())
        );
    }

    #[test]
    fn a_retry_with_a_model_records_it_and_a_retry_with_none_records_none() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(&journal, &clock(), &token, Some("opus"), false, true, false),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_model(&journal, TaskId(1), 1),
            Ok(Some("opus".to_owned()))
        );

        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(&journal, &clock(), &token, None, false, true, false),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_model(&journal, TaskId(1), 1),
            Ok(None)
        );
    }

    #[test]
    fn a_retry_with_reset_tree_records_it_and_a_retry_without_records_false() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(&journal, &clock(), &token, None, false, true, true),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_reset_tree(&journal, TaskId(1), 1),
            Ok(true)
        );

        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(&journal, &clock(), &token, None, false, true, false),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_reset_tree(&journal, TaskId(1), 1),
            Ok(false)
        );
    }

    #[test]
    fn retry_same_session_without_a_recorded_session_is_refused_naming_it() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        let error = report_retry(&journal, &clock(), &token, None, true, true, false).unwrap_err();
        assert_eq!(error, ReportError::NoSessionRecorded);
        assert!(
            error
                .to_string()
                .contains("reported no session to continue")
        );
        // Nothing was recorded: the attempt is still open for a later, valid report.
        assert_eq!(
            crate::attempt::last_retry_same_session(&journal, TaskId(1), 1),
            Ok(false)
        );
    }

    #[test]
    fn retry_same_session_for_a_provider_without_resume_support_is_refused_the_same_way() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        crate::attempt::record_session(&journal, &clock(), TaskId(1), 1, "a-session").unwrap();
        let error = report_retry(&journal, &clock(), &token, None, true, false, false).unwrap_err();
        assert_eq!(error, ReportError::ResumeNotSupported);
        assert!(error.to_string().contains("does not support resuming"));
    }

    #[test]
    fn retry_same_session_with_a_recorded_session_and_a_supporting_provider_is_recorded() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        crate::attempt::record_session(&journal, &clock(), TaskId(1), 1, "a-session").unwrap();
        assert_eq!(
            report_retry(&journal, &clock(), &token, None, true, true, false),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_same_session(&journal, TaskId(1), 1),
            Ok(true)
        );
    }

    #[test]
    fn a_retry_outside_the_resolve_step_is_refused_the_same_as_report_would() {
        let journal = journal_with_a_running_step(crate::IMPLEMENTATION);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        let error =
            report_retry(&journal, &clock(), &token, Some("opus"), false, true, false).unwrap_err();
        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Retry,
                step: crate::IMPLEMENTATION.to_owned(),
                expected: vec![
                    Outcome::Done,
                    Outcome::Failed,
                    Outcome::NeedsInput,
                    Outcome::TooLarge
                ],
            }
        );
        assert_eq!(
            crate::attempt::last_retry_model(&journal, TaskId(1), 1),
            Ok(None)
        );
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

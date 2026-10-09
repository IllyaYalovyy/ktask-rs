//! An agent stating the outcome of an attempt.

use std::error::Error;
use std::fmt;

use crate::queue_state::ReportDecision;
use crate::{Clock, Finding, ImportError, Journal, RecordReportError, TaskId};

mod outcome;
mod token;

pub use outcome::Outcome;
pub use token::AttemptToken;

use outcome::outcomes_for_step;

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
    /// `--more-time` was given, but the attempt the token names did not end at its time limit.
    NoTimeLimitEnding,
    /// `supersede`'s own tasks file is refused — the same way, with the same messages,
    /// [`crate::import_tasks`] refuses one.
    Import(ImportError),
    /// `changes-requested` was given no finding at all, or [`report`] was asked to record it
    /// directly instead of through [`report_findings`].
    FindingsRequired,
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
            Self::NoTimeLimitEnding => f.write_str(
                "this attempt did not end at its time limit: --more-time only follows a timeout",
            ),
            Self::Import(error) => error.fmt(f),
            Self::FindingsRequired => {
                f.write_str("outcome changes-requested needs at least one finding: pass --findings")
            }
        }
    }
}

impl Error for ReportError {}

impl From<RecordReportError> for ReportError {
    fn from(error: RecordReportError) -> Self {
        Self::Record(error)
    }
}

impl From<ImportError> for ReportError {
    fn from(error: ImportError) -> Self {
        Self::Import(error)
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
/// given; when `outcome` is `changes-requested`, which [`report_findings`] records instead;
/// when `outcome` does not belong to the step currently running for this attempt; or when the
/// journal reports the attempt is unknown or has ended.
pub fn report(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    outcome: Outcome,
    reason: Option<&str>,
) -> Result<(), ReportError> {
    if outcome == Outcome::ChangesRequested {
        return Err(ReportError::FindingsRequired);
    }
    report_impl(
        journal,
        clock,
        token,
        ReportDecision {
            outcome,
            reason,
            findings: &[],
            retry_model: None,
            retry_same_session: false,
            retry_reset_tree: false,
            retry_more_time: None,
        },
    )
}

/// Use case: records the reviewer's `changes-requested` verdict for the attempt `token` names,
/// carrying `findings` — the reviewer's own findings, a list the fixer, the operator and the
/// next reviewer all read the same way.
///
/// # Errors
///
/// Fails, recording nothing, when `findings` is empty ([`ReportError::FindingsRequired`]);
/// when `changes-requested` does not belong to the step currently running for this attempt —
/// only the review step accepts it; or when the journal reports the attempt is unknown or has
/// ended.
pub fn report_findings(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    findings: &[Finding],
) -> Result<(), ReportError> {
    if findings.is_empty() {
        return Err(ReportError::FindingsRequired);
    }
    report_impl(
        journal,
        clock,
        token,
        ReportDecision {
            outcome: Outcome::ChangesRequested,
            reason: None,
            findings,
            retry_model: None,
            retry_same_session: false,
            retry_reset_tree: false,
            retry_more_time: None,
        },
    )
}

/// What `--retry` asks for: the model it names for the task's next attempt, when it names one,
/// whether that next attempt should resume this one's own session instead of starting fresh,
/// whether the working tree should be returned to the commit this attempt started from first,
/// and how many extra minutes to give that next attempt, when any — [`report_retry`]'s own
/// request, built by its callers.
#[derive(Debug, Clone, Copy)]
pub struct RetryRequest<'a> {
    /// The model named for the task's next attempt, when one was named.
    pub model: Option<&'a str>,
    /// Whether the task's next attempt should resume this attempt's own session.
    pub same_session: bool,
    /// Whether the working tree should be reset to the commit this attempt started from.
    pub reset_tree: bool,
    /// How many extra minutes to give the task's next attempt, when any.
    pub more_time: Option<u32>,
}

/// Use case: records the resolver's `retry` decision for the attempt `token` names, carrying
/// `request` — the same as [`report`] with [`Outcome::Retry`], but with nowhere else for its
/// own fields to be given. `provider_supports_resume` is whatever the project's configured
/// provider reports for itself, checked only when `request.same_session` asks to resume one.
///
/// # Errors
///
/// Fails, recording nothing, when `retry` does not belong to the step currently running for
/// this attempt — only the resolve step accepts it; when the journal reports the attempt is
/// unknown or has ended; when `request.same_session` is asked for a provider that does not
/// support resuming at all ([`ReportError::ResumeNotSupported`]); or when `request.same_session`
/// is asked and this attempt reported no session to continue
/// ([`ReportError::NoSessionRecorded`]); or when `request.more_time` is given and this attempt
/// did not end at its time limit ([`ReportError::NoTimeLimitEnding`]).
pub fn report_retry(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    request: RetryRequest<'_>,
    provider_supports_resume: bool,
) -> Result<(), ReportError> {
    if request.more_time.is_some()
        && !crate::attempt::ended_at_time_limit(journal, token.task, token.number)
            .map_err(RecordReportError::from)?
    {
        return Err(ReportError::NoTimeLimitEnding);
    }
    if request.same_session {
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
        ReportDecision {
            outcome: Outcome::Retry,
            reason: None,
            findings: &[],
            retry_model: request.model,
            retry_same_session: request.same_session,
            retry_reset_tree: request.reset_tree,
            retry_more_time: request.more_time,
        },
    )
}

/// [`report`], [`report_findings`] and [`report_retry`]'s shared work: all three are this,
/// differing only in whether `decision.findings` is ever non-empty, `decision.retry_model` is
/// ever anything but `None`, and `decision.retry_same_session` and `decision.retry_reset_tree`
/// ever `true`.
fn report_impl(
    journal: &impl Journal,
    clock: &impl Clock,
    token: &AttemptToken,
    decision: ReportDecision<'_>,
) -> Result<(), ReportError> {
    let blank = decision
        .reason
        .is_none_or(|reason| reason.trim().is_empty());
    if decision.outcome.needs_reason() && blank {
        return Err(ReportError::ReasonRequired(decision.outcome));
    }
    supersede::check_outcome_for_step(journal, token, decision.outcome)?;
    crate::attempt::record_report(journal, clock, token.task, token.number, decision)?;
    Ok(())
}

mod supersede;

pub use supersede::{Supersede, report_supersede};

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{BeginAttemptError, FindingScope, Placement, TaskId, TaskStatus, add_task};

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
        crate::attempt::begin_step(&journal, &clock(), TaskId(1), 1, step, None, None).unwrap();
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
    fn approved_needs_no_reason() {
        let journal = journal_with_a_running_step(crate::REVIEW_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report(&journal, &clock(), &token, Outcome::Approved, None),
            Ok(())
        );
    }

    /// A finding with `location` and nothing else that matters.
    fn finding(location: &str) -> Finding {
        Finding {
            location: location.to_owned(),
            problem: "it is wrong".to_owned(),
            fix: "fix it".to_owned(),
            scope: FindingScope::Task,
        }
    }

    #[test]
    fn report_refuses_changes_requested_directly_naming_report_findings() {
        let journal = journal_with_a_running_step(crate::REVIEW_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        for reason in [None, Some("fix this")] {
            assert_eq!(
                report(
                    &journal,
                    &clock(),
                    &token,
                    Outcome::ChangesRequested,
                    reason
                ),
                Err(ReportError::FindingsRequired)
            );
        }
    }

    #[test]
    fn report_findings_is_refused_empty_and_recorded_once_given_any() {
        let journal = journal_with_a_running_step(crate::REVIEW_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_findings(&journal, &clock(), &token, &[]),
            Err(ReportError::FindingsRequired)
        );
        assert_eq!(
            report_findings(&journal, &clock(), &token, &[finding("src/a.rs:1")]),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_report(&journal, TaskId(1), 1),
            Ok(Some((Outcome::ChangesRequested, None)))
        );
    }

    #[test]
    fn report_findings_outside_the_review_step_is_refused_naming_it() {
        let journal = journal_with_a_running_step(crate::IMPLEMENTATION);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_findings(&journal, &clock(), &token, &[finding("src/a.rs:1")]),
            Err(ReportError::WrongStep {
                outcome: Outcome::ChangesRequested,
                step: crate::IMPLEMENTATION.to_owned(),
                expected: vec![
                    Outcome::Done,
                    Outcome::Failed,
                    Outcome::NeedsInput,
                    Outcome::TooLarge
                ],
            })
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

    /// A [`RetryRequest`] naming `model`, with `same_session` and `reset_tree` as given, and no
    /// extra time — every retry test below varies only these three.
    fn retry_request(
        model: Option<&str>,
        same_session: bool,
        reset_tree: bool,
    ) -> RetryRequest<'_> {
        RetryRequest {
            model,
            same_session,
            reset_tree,
            more_time: None,
        }
    }

    #[test]
    fn a_retry_with_a_model_records_it_and_a_retry_with_none_records_none() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(
                &journal,
                &clock(),
                &token,
                retry_request(Some("opus"), false, false),
                true
            ),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_model(&journal, TaskId(1), 1),
            Ok(Some("opus".to_owned()))
        );

        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(
                &journal,
                &clock(),
                &token,
                retry_request(None, false, false),
                true
            ),
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
            report_retry(
                &journal,
                &clock(),
                &token,
                retry_request(None, false, true),
                true
            ),
            Ok(())
        );
        assert_eq!(
            crate::attempt::last_retry_reset_tree(&journal, TaskId(1), 1),
            Ok(true)
        );

        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        assert_eq!(
            report_retry(
                &journal,
                &clock(),
                &token,
                retry_request(None, false, false),
                true
            ),
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
        let error = report_retry(
            &journal,
            &clock(),
            &token,
            retry_request(None, true, false),
            true,
        )
        .unwrap_err();
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
        let error = report_retry(
            &journal,
            &clock(),
            &token,
            retry_request(None, true, false),
            false,
        )
        .unwrap_err();
        assert_eq!(error, ReportError::ResumeNotSupported);
        assert!(error.to_string().contains("does not support resuming"));
    }

    #[test]
    fn retry_same_session_with_a_recorded_session_and_a_supporting_provider_is_recorded() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        crate::attempt::record_session(&journal, &clock(), TaskId(1), 1, "a-session").unwrap();
        assert_eq!(
            report_retry(
                &journal,
                &clock(),
                &token,
                retry_request(None, true, false),
                true
            ),
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
        let error = report_retry(
            &journal,
            &clock(),
            &token,
            retry_request(Some("opus"), false, false),
            true,
        )
        .unwrap_err();
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
    fn a_skip_inside_the_resolve_step_needs_a_reason_and_is_recorded_once_given() {
        let journal = journal_with_a_running_step(crate::RESOLVE_STEP);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        for blank in [None, Some(""), Some("   ")] {
            assert_eq!(
                report(&journal, &clock(), &token, Outcome::Skip, blank),
                Err(ReportError::ReasonRequired(Outcome::Skip))
            );
        }
        assert_eq!(
            report(
                &journal,
                &clock(),
                &token,
                Outcome::Skip,
                Some("no longer relevant")
            ),
            Ok(())
        );
    }

    #[test]
    fn a_skip_outside_the_resolve_step_is_refused_naming_retry_stop_and_skip() {
        let journal = journal_with_a_running_step(crate::IMPLEMENTATION);
        let token = AttemptToken::new("proj", TaskId(1), 1);
        let error = report(
            &journal,
            &clock(),
            &token,
            Outcome::Skip,
            Some("no longer relevant"),
        )
        .unwrap_err();
        assert_eq!(
            error,
            ReportError::WrongStep {
                outcome: Outcome::Skip,
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

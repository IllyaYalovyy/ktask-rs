//! Parsing an agent's report into a typed result, per `VISION.md` §3
//! invariant 4: "A task is never done based only on an agent exit code or
//! statement."
//!
//! [`parse_report`] only answers "what did the agent claim?" — it takes a
//! report's text and nothing else, so it has no way to consult a gate
//! result even if it wanted to. Deciding whether a task is actually done
//! stays with the caller, which must have independently run the mandatory
//! gates before it trusts a [`ReportResult::Done`]. That separation is
//! structural, not a convention this module could accidentally violate.
//!
//! The header format matches `AGENTS.md`'s reporting contract: the first
//! non-empty line must be exactly `KTASK_RESULT: DONE`, `KTASK_RESULT:
//! FAILED` or `KTASK_RESULT: NEEDS_INPUT` — no heading marker, no leading
//! or trailing text on that line.
//!
//! `waiting_input` must carry a real question (`VISION.md` §6, invariant
//! 8): a `NEEDS_INPUT` header's body is parsed into a
//! [`crate::DecisionRequest`] by [`crate::parse_decision_request`], and a
//! report that claims `NEEDS_INPUT` without one is rejected as malformed
//! rather than accepted as an empty pause.

use crate::ids::{AttemptId, TaskId};
use crate::project::Project;
use crate::{Error, Result, decision};
use std::path::PathBuf;

/// What an agent's report claimed about the outcome of its task.
///
/// This is only a claim. `VISION.md` §3 invariant 4 forbids treating
/// [`ReportResult::Done`] as completion on its own; a caller must confirm
/// it against a mechanical gate result before acting on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportResult {
    /// The agent claimed the task's objective and evidence were satisfied.
    Done,
    /// The agent could not complete the task and stopped.
    Failed,
    /// The agent stopped because a human decision is required, carrying the
    /// structured question parsed from the report's body.
    NeedsInput(decision::DecisionRequest),
}

const EXPECTED: &str =
    "\"KTASK_RESULT: DONE\", \"KTASK_RESULT: FAILED\" or \"KTASK_RESULT: NEEDS_INPUT\"";

/// Parses an agent's report text into a [`ReportResult`].
///
/// The first non-empty line of `text` must be exactly `KTASK_RESULT: DONE`,
/// `KTASK_RESULT: FAILED` or `KTASK_RESULT: NEEDS_INPUT` — nothing before
/// it on the line, nothing after. A report with no non-empty line, or whose
/// first non-empty line is anything else, is an error naming what was
/// expected and what was found instead.
///
/// For `KTASK_RESULT: NEEDS_INPUT`, everything after the header line is
/// parsed as a decision request (see [`crate::parse_decision_request`]); a
/// body missing its question, options, trade-offs or impact is rejected
/// rather than treated as a pause with nothing to show a human.
///
/// # Errors
///
/// Returns [`Error::Report`] if `text` has no non-empty line, if its first
/// non-empty line is not exactly one of the three permitted headers, or if
/// a `NEEDS_INPUT` header's body is missing a required decision section.
pub fn parse_report(text: &str) -> Result<ReportResult> {
    let mut lines = text.lines();
    let header = lines
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| Error::Report {
            detail: format!("empty report: expected a first line of {EXPECTED}"),
        })?;

    match header {
        "KTASK_RESULT: DONE" => Ok(ReportResult::Done),
        "KTASK_RESULT: FAILED" => Ok(ReportResult::Failed),
        "KTASK_RESULT: NEEDS_INPUT" => {
            let body = lines.collect::<Vec<_>>().join("\n");
            let request = decision::parse_decision_request(&body)?;
            Ok(ReportResult::NeedsInput(request))
        }
        other => Err(Error::Report {
            detail: format!("malformed result header: expected {EXPECTED}, found {other:?}"),
        }),
    }
}

/// Where `attempt`'s report for `task` must be written, and is read back
/// from: `<state_dir>/attempts/<task>/<attempt>/report.md`.
///
/// Keyed by both ids, not just the attempt's, since [`AttemptId`] only
/// counts within its own task: two different tasks' first attempts would
/// otherwise collide on the same path. Naming the attempt in the path is
/// also what keeps a retry's report from ever being mistaken for an earlier
/// attempt's: each attempt gets its own file, never overwriting a prior
/// one's.
#[must_use]
pub fn report_path(project: &Project, task: TaskId, attempt: AttemptId) -> PathBuf {
    project
        .state_dir
        .join("attempts")
        .join(task.get().to_string())
        .join(attempt.get().to_string())
        .join("report.md")
}

/// Creates the directory [`report_path`] names, so it exists before the
/// provider that is expected to write into it ever runs. Returns the path
/// itself, so a caller can pass it straight into the prompt it assembles for
/// that provider.
///
/// # Errors
///
/// Returns [`Error::Io`] if the directory cannot be created.
pub fn ensure_report_dir(project: &Project, task: TaskId, attempt: AttemptId) -> Result<PathBuf> {
    let path = report_path(project, task, attempt);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(path)
}

/// Reads back and parses the report a provider wrote for `attempt` at
/// [`report_path`], per `VISION.md` §3 invariant 4: an attempt's outcome is
/// never assumed from the provider merely exiting.
///
/// # Errors
///
/// Returns [`Error::Report`] naming `report_path`'s value if no file exists
/// there at all — a provider exiting without writing a report is a claim of
/// nothing, not of success — and whatever [`parse_report`] itself returns if
/// the file exists but is malformed.
pub fn read_report(project: &Project, task: TaskId, attempt: AttemptId) -> Result<ReportResult> {
    let path = report_path(project, task, attempt);
    let text = std::fs::read_to_string(&path).map_err(|source| Error::Report {
        detail: format!(
            "no report found at {}: expected a first line of {EXPECTED} ({source})",
            path.display()
        ),
    })?;
    parse_report(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_header_parses_to_done() {
        let report = "KTASK_RESULT: DONE\nSummary: it worked.\n";
        assert_eq!(parse_report(report).unwrap(), ReportResult::Done);
    }

    #[test]
    fn failed_header_parses_to_failed() {
        let report = "KTASK_RESULT: FAILED\nReason: gate would not go green.\n";
        assert_eq!(parse_report(report).unwrap(), ReportResult::Failed);
    }

    #[test]
    fn needs_input_header_parses_the_body_into_a_decision_request() {
        let report = "KTASK_RESULT: NEEDS_INPUT\n\
                       Question: Postgres or SQLite for the journal?\n\
                       Options:\n\
                       - Postgres\n\
                       - SQLite\n\
                       Trade-offs: Postgres scales better; SQLite is simpler to run.\n\
                       Impact: Journal durability and operational overhead.\n\
                       Recommended: SQLite\n";
        assert_eq!(
            parse_report(report).unwrap(),
            ReportResult::NeedsInput(decision::DecisionRequest {
                question: "Postgres or SQLite for the journal?".to_string(),
                options: vec!["Postgres".to_string(), "SQLite".to_string()],
                tradeoffs: "Postgres scales better; SQLite is simpler to run.".to_string(),
                impact: "Journal durability and operational overhead.".to_string(),
                recommended: Some("SQLite".to_string()),
            })
        );
    }

    #[test]
    fn needs_input_header_with_no_question_is_a_malformed_report_not_a_pause() {
        let report = "KTASK_RESULT: NEEDS_INPUT\nAction: pick a migration strategy.\n";
        let err = parse_report(report).expect_err("no Question section is malformed");
        assert!(err.to_string().contains("Question"));
    }

    #[test]
    fn leading_blank_lines_are_skipped_to_find_the_header() {
        let report = "\n\n   \nKTASK_RESULT: DONE\n";
        assert_eq!(parse_report(report).unwrap(), ReportResult::Done);
    }

    #[test]
    fn empty_report_is_an_error_naming_the_expected_header() {
        let err = parse_report("").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("KTASK_RESULT: DONE"));
        assert!(message.contains("KTASK_RESULT: FAILED"));
        assert!(message.contains("KTASK_RESULT: NEEDS_INPUT"));
    }

    #[test]
    fn blank_only_report_is_an_error() {
        assert!(parse_report("\n\n   \n").is_err());
    }

    #[test]
    fn heading_marker_before_the_result_is_malformed() {
        let err = parse_report("## KTASK_RESULT: DONE\n").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("KTASK_RESULT: DONE"));
        assert!(message.contains("## KTASK_RESULT: DONE"));
    }

    #[test]
    fn unrecognized_result_value_is_malformed() {
        let err = parse_report("KTASK_RESULT: MAYBE\n").unwrap_err();
        assert!(err.to_string().contains("MAYBE"));
    }

    #[test]
    fn trailing_text_on_the_header_line_is_malformed() {
        let err = parse_report("KTASK_RESULT: DONE and also great\n").unwrap_err();
        assert!(err.to_string().contains("DONE and also great"));
    }

    #[test]
    fn lowercase_result_is_malformed() {
        assert!(parse_report("ktask_result: done\n").is_err());
    }

    #[test]
    fn missing_space_after_colon_is_malformed() {
        assert!(parse_report("KTASK_RESULT:DONE\n").is_err());
    }

    fn project_at(state_dir: &std::path::Path) -> Project {
        Project {
            root: PathBuf::from("/repo"),
            id: "test-project".to_string(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn report_path_is_keyed_by_state_dir_task_and_attempt() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());

        let path = report_path(&project, TaskId::new(7), AttemptId::new(2));

        assert_eq!(
            path,
            state
                .path()
                .join("attempts")
                .join("7")
                .join("2")
                .join("report.md")
        );
    }

    #[test]
    fn ensure_report_dir_creates_the_directory_report_path_lives_in() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());

        let path = ensure_report_dir(&project, TaskId::new(1), AttemptId::new(1))
            .expect("ensure_report_dir");

        assert!(
            path.parent().expect("report.md has a parent").is_dir(),
            "the report's directory must exist before anything writes into it"
        );
        assert_eq!(
            path,
            report_path(&project, TaskId::new(1), AttemptId::new(1))
        );
    }

    #[test]
    fn read_report_round_trips_a_report_written_at_report_path() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let task = TaskId::new(1);
        let attempt = AttemptId::new(1);

        let path = ensure_report_dir(&project, task, attempt).expect("ensure_report_dir");
        std::fs::write(&path, "KTASK_RESULT: DONE\nSummary: it worked.\n").expect("write report");

        let result = read_report(&project, task, attempt).expect("read_report");

        assert_eq!(result, ReportResult::Done);
    }

    #[test]
    fn read_report_for_a_missing_file_is_a_classified_failure_naming_the_expected_path() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let task = TaskId::new(1);
        let attempt = AttemptId::new(1);
        let expected_path = report_path(&project, task, attempt);

        let err = read_report(&project, task, attempt)
            .expect_err("no report was ever written for this attempt");

        assert!(
            matches!(err, Error::Report { .. }),
            "a missing report must be a classified Error::Report, not treated as success"
        );
        assert!(
            err.to_string()
                .contains(&expected_path.display().to_string()),
            "error must name the expected path, got: {err}"
        );
    }

    #[test]
    fn a_second_attempts_missing_report_is_never_mistaken_for_the_first_attempts_report() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let task = TaskId::new(1);
        let first = AttemptId::new(1);
        let second = AttemptId::new(2);

        let first_path = ensure_report_dir(&project, task, first).expect("ensure_report_dir");
        std::fs::write(&first_path, "KTASK_RESULT: DONE\nSummary: first attempt.\n")
            .expect("write first attempt's report");

        // The second attempt never wrote its own report; reading it must
        // fail rather than silently returning the first attempt's `Done`.
        let err = read_report(&project, task, second)
            .expect_err("the second attempt's report was never written");
        assert!(matches!(err, Error::Report { .. }));

        // The first attempt's report is unaffected and still reads back.
        let first_result = read_report(&project, task, first).expect("read first attempt");
        assert_eq!(first_result, ReportResult::Done);
    }

    #[test]
    fn read_report_distinguishes_two_attempts_that_both_wrote_reports() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let task = TaskId::new(1);
        let first = AttemptId::new(1);
        let second = AttemptId::new(2);

        let first_path = ensure_report_dir(&project, task, first).expect("ensure_report_dir");
        std::fs::write(
            &first_path,
            "KTASK_RESULT: FAILED\nReason: first attempt failed.\n",
        )
        .expect("write first attempt's report");
        let second_path = ensure_report_dir(&project, task, second).expect("ensure_report_dir");
        std::fs::write(
            &second_path,
            "KTASK_RESULT: DONE\nSummary: second attempt worked.\n",
        )
        .expect("write second attempt's report");

        assert_eq!(
            read_report(&project, task, first).expect("read first"),
            ReportResult::Failed
        );
        assert_eq!(
            read_report(&project, task, second).expect("read second"),
            ReportResult::Done
        );
    }
}

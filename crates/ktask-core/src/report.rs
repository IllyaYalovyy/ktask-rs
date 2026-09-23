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

use crate::{Error, Result, decision};

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
}

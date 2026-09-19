//! Report parsing and result contract.
//!
//! An agent's report is parsed into a typed result. The first non-empty line must be
//! exactly `KTASK_RESULT: DONE`, `FAILED` or `NEEDS_INPUT`.

use crate::Error;

/// Result of parsing an agent's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportResult {
    /// Task completed successfully.
    Done,
    /// Task failed.
    Failed,
    /// Task requires human input.
    NeedsInput,
}

/// Parse an agent's report into a typed result.
///
/// The first non-empty line must be exactly `KTASK_RESULT: DONE`, `FAILED` or `NEEDS_INPUT`.
/// If the header is missing or malformed, returns an error naming what was expected.
///
/// # Arguments
/// * `text` - The full report text.
///
/// # Errors
/// Returns an error if the first non-empty line is not exactly one of the three expected
/// headers, with a clear message naming the valid options.
pub fn parse_report(text: &str) -> crate::Result<ReportResult> {
    let first_non_empty_line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| Error::Deserialize {
            detail: "Report is empty or contains only whitespace. Expected first non-empty line to be exactly: KTASK_RESULT: DONE, KTASK_RESULT: FAILED, or KTASK_RESULT: NEEDS_INPUT".to_string(),
        })?;

    match first_non_empty_line.trim() {
        "KTASK_RESULT: DONE" => Ok(ReportResult::Done),
        "KTASK_RESULT: FAILED" => Ok(ReportResult::Failed),
        "KTASK_RESULT: NEEDS_INPUT" => Ok(ReportResult::NeedsInput),
        other => Err(Error::Deserialize {
            detail: format!(
                "Invalid report header: '{other}'. Expected first non-empty line to be exactly one of: KTASK_RESULT: DONE, KTASK_RESULT: FAILED, KTASK_RESULT: NEEDS_INPUT"
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_report_done() {
        let report = "KTASK_RESULT: DONE\nSome details here.";
        let result = parse_report(report).expect("should parse");
        assert_eq!(result, ReportResult::Done);
    }

    #[test]
    fn parse_report_failed() {
        let report = "KTASK_RESULT: FAILED\nFailure reason.";
        let result = parse_report(report).expect("should parse");
        assert_eq!(result, ReportResult::Failed);
    }

    #[test]
    fn parse_report_needs_input() {
        let report = "KTASK_RESULT: NEEDS_INPUT\nQuestion for the user.";
        let result = parse_report(report).expect("should parse");
        assert_eq!(result, ReportResult::NeedsInput);
    }

    #[test]
    fn parse_report_skips_leading_whitespace() {
        let report = "\n\n   \nKTASK_RESULT: DONE\nDetails.";
        let result = parse_report(report).expect("should parse");
        assert_eq!(result, ReportResult::Done);
    }

    #[test]
    fn parse_report_error_on_empty() {
        let report = "";
        let err = parse_report(report).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("KTASK_RESULT"));
        assert!(msg.contains("empty"));
    }

    #[test]
    fn parse_report_error_on_whitespace_only() {
        let report = "   \n  \n  ";
        let err = parse_report(report).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("KTASK_RESULT"));
    }

    #[test]
    fn parse_report_error_on_malformed_header() {
        let report = "KTASK_RESULT: DONEE\nDetails.";
        let err = parse_report(report).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Invalid report header"));
        assert!(msg.contains("DONEE"));
    }

    #[test]
    fn parse_report_error_on_wrong_prefix() {
        let report = "TASK_RESULT: DONE\nDetails.";
        let err = parse_report(report).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Invalid report header"));
    }

    #[test]
    fn parse_report_error_on_lowercase() {
        let report = "ktask_result: done\nDetails.";
        let err = parse_report(report).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Invalid report header"));
    }

    #[test]
    fn parse_report_with_multiline_content() {
        let report = "KTASK_RESULT: DONE\nLine 1\nLine 2\nLine 3";
        let result = parse_report(report).expect("should parse");
        assert_eq!(result, ReportResult::Done);
    }

    #[test]
    fn parse_report_error_naming_what_was_expected() {
        let report = "Invalid header";
        let err = parse_report(report).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Expected"));
        assert!(msg.contains("DONE"));
        assert!(msg.contains("FAILED"));
        assert!(msg.contains("NEEDS_INPUT"));
    }
}

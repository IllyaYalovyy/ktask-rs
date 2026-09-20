//! Report parsing and result contract.
//!
//! An agent's report is parsed into a typed result. The first non-empty line must be
//! exactly `KTASK_RESULT: DONE`, `FAILED` or `NEEDS_INPUT`.

use crate::{AttemptId, Error, Project, TaskId};
use std::path::PathBuf;

/// Get the expected report path for an attempt.
///
/// Returns `<state_dir>/attempts/<task>/<attempt>/report.md`.
///
/// # Arguments
/// * `project` - The registered ktask project
/// * `task` - The task identifier
/// * `attempt` - The attempt identifier
#[must_use]
pub fn report_path(project: &Project, task: TaskId, attempt: AttemptId) -> PathBuf {
    project
        .state_dir
        .join("attempts")
        .join(task.to_string())
        .join(attempt.to_string())
        .join("report.md")
}

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

    #[test]
    fn report_path_returns_correct_structure() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let path = report_path(&project, TaskId::new(1), AttemptId::new(1));

        let expected = state_dir
            .join("attempts")
            .join("1")
            .join("1")
            .join("report.md");

        assert_eq!(path, expected);
    }

    #[test]
    fn report_path_differs_for_different_attempts() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir,
        };

        let path1 = report_path(&project, TaskId::new(1), AttemptId::new(1));
        let path2 = report_path(&project, TaskId::new(1), AttemptId::new(2));

        assert_ne!(path1, path2);
        assert!(path1.ends_with("1/report.md"));
        assert!(path2.ends_with("2/report.md"));
    }

    #[test]
    fn report_path_differs_for_different_tasks() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir,
        };

        let path1 = report_path(&project, TaskId::new(1), AttemptId::new(1));
        let path2 = report_path(&project, TaskId::new(2), AttemptId::new(1));

        assert_ne!(path1, path2);
        // Both should end with report.md but be in different task directories
        assert!(path1.ends_with("1/1/report.md"));
        assert!(path2.ends_with("2/1/report.md"));
    }

    #[test]
    fn report_path_ends_with_report_md() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir,
        };

        let path = report_path(&project, TaskId::new(5), AttemptId::new(3));

        assert!(path.ends_with("report.md"));
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("attempts/5/3/report.md"));
    }
}

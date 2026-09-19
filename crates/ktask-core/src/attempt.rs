//! Attempt records: complete evidence of every attempt at a task.

use crate::Result;
use crate::{AttemptId, GateResult, Project, TaskId, Usage, redact};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use time::OffsetDateTime;

/// Complete record of a single attempt at a task, including all evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// Attempt identifier.
    pub id: AttemptId,
    /// Task identifier.
    pub task: TaskId,
    /// Timestamp when the attempt started.
    pub started: OffsetDateTime,
    /// Timestamp when the attempt ended, if applicable.
    pub ended: Option<OffsetDateTime>,
    /// Configured model ID for this attempt.
    pub model_configured: Option<String>,
    /// Model ID reported by the provider.
    pub model_reported: Option<String>,
    /// Provider session identifier.
    pub session_id: Option<String>,
    /// Reason the attempt ended (exit reason, failure classification, etc.).
    pub exit_reason: String,
    /// Gate results from this attempt.
    pub gates: Vec<GateResult>,
    /// Token and cost usage for this attempt.
    pub usage: Option<Usage>,
    /// Base commit SHA at the start of this attempt.
    pub base_sha: String,
    /// Candidate commit SHA if the attempt reached publication.
    pub candidate_sha: Option<String>,
}

/// Write attempt evidence to disk.
///
/// Stores the attempt record and context in a structured layout at:
/// `<state_dir>/attempts/<task>/<attempt>/`
///
/// Files created:
/// - `record.json`: Serialized [`AttemptRecord`] (redacted)
/// - `context.md`: Task context document (redacted)
/// - `report.md`: Empty placeholder (future use)
/// - `gates/<kind>.log`: Individual gate output files (redacted)
///
/// Directories are created with mode 0700 for privacy.
///
/// # Errors
///
/// Returns an error if the directories cannot be created or if
/// serialization/writing fails.
pub fn write_evidence(project: &Project, record: &AttemptRecord, context: &str) -> Result<()> {
    let evidence_root = project.state_dir.join("attempts");
    let task_dir = evidence_root.join(record.task.to_string());
    let attempt_dir = task_dir.join(record.id.to_string());

    // Create directories with restrictive permissions
    create_dir_with_mode(&evidence_root)?;
    create_dir_with_mode(&task_dir)?;
    create_dir_with_mode(&attempt_dir)?;

    let gates_dir = attempt_dir.join("gates");
    create_dir_with_mode(&gates_dir)?;

    // Write record.json (redacted)
    let record_json = serde_json::to_string_pretty(&record)?;
    let redacted_record = redact::redact(&record_json, &[]);
    fs::write(attempt_dir.join("record.json"), redacted_record)?;

    // Write context.md (redacted)
    let redacted_context = redact::redact(context, &[]);
    fs::write(attempt_dir.join("context.md"), redacted_context)?;

    // Write report.md (placeholder)
    fs::write(attempt_dir.join("report.md"), "")?;

    // Write individual gate logs (redacted)
    for gate_result in &record.gates {
        let gate_kind = format!("{:?}", gate_result.kind).to_lowercase();
        let gate_output = format!(
            "Exit code: {}\nTimeout: {}\nDuration: {}ms\n\n--- STDOUT ---\n{}\n\n--- STDERR ---\n{}",
            gate_result.exit_code.unwrap_or(-1),
            gate_result.timed_out,
            gate_result.duration_ms,
            gate_result.stdout,
            gate_result.stderr
        );
        let redacted_output = redact::redact(&gate_output, &[]);
        fs::write(gates_dir.join(format!("{gate_kind}.log")), redacted_output)?;
    }

    Ok(())
}

/// Read attempt records for a task from disk.
///
/// Reads all `record.json` files from `<state_dir>/attempts/<task>/*/`
/// and deserializes them into [`AttemptRecord`] objects, ordered by attempt ID.
///
/// # Errors
///
/// Returns an error if the attempts directory doesn't exist, or if
/// deserialization fails.
pub fn read_evidence(project: &Project, task: TaskId) -> Result<Vec<AttemptRecord>> {
    let task_dir = project.state_dir.join("attempts").join(task.to_string());

    if !task_dir.exists() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();

    // Read all attempt directories
    for entry in fs::read_dir(&task_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let record_path = path.join("record.json");
            if record_path.exists() {
                let json_content = fs::read_to_string(&record_path)?;
                let record: AttemptRecord = serde_json::from_str(&json_content)?;
                records.push(record);
            }
        }
    }

    // Sort by attempt ID to ensure consistent ordering
    records.sort_by_key(|r| r.id);
    Ok(records)
}

/// Create a directory with mode 0700.
///
/// Creates the directory if it doesn't exist. On Unix systems, sets
/// restrictive permissions (0700). On non-Unix systems, creates the
/// directory normally.
fn create_dir_with_mode(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir(path)?;
        let perms = Permissions::from_mode(0o700);
        fs::set_permissions(path, perms)?;
    }

    #[cfg(not(unix))]
    {
        fs::create_dir(path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AttemptId, TaskId};
    use std::path::PathBuf;
    use time::OffsetDateTime;

    #[test]
    fn attempt_record_roundtrips_through_json() {
        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(1),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-123".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        let json = serde_json::to_string(&record).expect("serialize");
        let deserialized: AttemptRecord = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(record.id, deserialized.id);
        assert_eq!(record.task, deserialized.task);
        assert_eq!(record.model_configured, deserialized.model_configured);
        assert_eq!(record.model_reported, deserialized.model_reported);
        assert_eq!(record.session_id, deserialized.session_id);
        assert_eq!(record.exit_reason, deserialized.exit_reason);
        assert_eq!(record.base_sha, deserialized.base_sha);
        assert_eq!(record.candidate_sha, deserialized.candidate_sha);
    }

    #[test]
    fn attempt_record_with_minimal_fields() {
        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(1),
            started: now,
            ended: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            exit_reason: "interrupted".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: None,
        };

        let json = serde_json::to_string(&record).expect("serialize");
        let deserialized: AttemptRecord = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(record, deserialized);
    }

    #[test]
    fn write_evidence_creates_directory_structure() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(5),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-123".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        let context = "# Task Context\n\nSome task details";

        write_evidence(&project, &record, context).expect("write evidence");

        // Verify directory structure
        let evidence_root = state_dir.join("attempts");
        assert!(evidence_root.exists());

        let task_dir = evidence_root.join("5");
        assert!(task_dir.exists());

        let attempt_dir = task_dir.join("1");
        assert!(attempt_dir.exists());

        let gates_dir = attempt_dir.join("gates");
        assert!(gates_dir.exists());

        // Verify files exist
        assert!(attempt_dir.join("record.json").exists());
        assert!(attempt_dir.join("context.md").exists());
        assert!(attempt_dir.join("report.md").exists());
    }

    #[test]
    fn write_evidence_redacts_sensitive_data() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(1),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("ghp_1234567890abcdefghijklmnopqrstuvwxyz".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        let context = "# Task Context\n\nAPI key: sk-1234567890abcdefghij";

        write_evidence(&project, &record, context).expect("write evidence");

        // Read back and verify redaction
        let attempt_dir = state_dir.join("attempts").join("1").join("1");
        let record_json =
            fs::read_to_string(attempt_dir.join("record.json")).expect("read record.json");
        let context_md =
            fs::read_to_string(attempt_dir.join("context.md")).expect("read context.md");

        // GitHub token should be redacted
        assert!(!record_json.contains("ghp_"));
        assert!(record_json.contains("[redacted]"));

        // API key should be redacted
        assert!(!context_md.contains("sk-"));
        assert!(context_md.contains("[redacted]"));
    }

    #[test]
    fn write_evidence_with_gate_results() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let now = OffsetDateTime::now_utc();
        let gate_result = GateResult {
            kind: crate::GateKind::Verify,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 5000,
            stdout: "test passed".to_string(),
            stderr: "".to_string(),
            timed_out: false,
        };

        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(1),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-123".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![gate_result],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        let context = "# Task Context";

        write_evidence(&project, &record, context).expect("write evidence");

        let gates_dir = state_dir.join("attempts").join("1").join("1").join("gates");
        assert!(gates_dir.join("verify.log").exists());

        let gate_log = fs::read_to_string(gates_dir.join("verify.log")).expect("read verify.log");
        assert!(gate_log.contains("Exit code: 0"));
        assert!(gate_log.contains("Timeout: false"));
        assert!(gate_log.contains("test passed"));
    }

    #[test]
    fn read_evidence_returns_empty_for_missing_task() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: temp_dir.path().to_path_buf(),
        };

        let records = read_evidence(&project, TaskId::new(999)).expect("read evidence");

        assert_eq!(records.len(), 0);
    }

    #[test]
    fn read_evidence_returns_all_attempts() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let now = OffsetDateTime::now_utc();

        // Write first attempt
        let record1 = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(5),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-1".to_string()),
            exit_reason: "failed".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: None,
        };

        write_evidence(&project, &record1, "Context 1").expect("write evidence 1");

        // Write second attempt
        let record2 = AttemptRecord {
            id: AttemptId::new(2),
            task: TaskId::new(5),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-2".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        write_evidence(&project, &record2, "Context 2").expect("write evidence 2");

        // Read all attempts
        let records = read_evidence(&project, TaskId::new(5)).expect("read evidence");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, AttemptId::new(1));
        assert_eq!(records[1].id, AttemptId::new(2));
        assert_eq!(records[0].exit_reason, "failed");
        assert_eq!(records[1].exit_reason, "success");
    }

    #[test]
    fn retry_adds_directory_rather_than_overwriting() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let now = OffsetDateTime::now_utc();

        // First attempt
        let record1 = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(3),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-1".to_string()),
            exit_reason: "failed".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: None,
        };

        write_evidence(&project, &record1, "Context 1").expect("write evidence 1");

        let attempt1_path = state_dir.join("attempts").join("3").join("1");
        assert!(attempt1_path.exists());

        // Second attempt (retry)
        let record2 = AttemptRecord {
            id: AttemptId::new(2),
            task: TaskId::new(3),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-2".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        write_evidence(&project, &record2, "Context 2").expect("write evidence 2");

        let attempt2_path = state_dir.join("attempts").join("3").join("2");

        // Both directories should exist
        assert!(
            attempt1_path.exists(),
            "First attempt directory should still exist"
        );
        assert!(
            attempt2_path.exists(),
            "Second attempt directory should exist"
        );

        // Verify they are different directories
        assert_ne!(
            fs::read_to_string(attempt1_path.join("record.json")).unwrap(),
            fs::read_to_string(attempt2_path.join("record.json")).unwrap()
        );
    }

    #[test]
    fn evidence_survives_rebuild_of_materialized_state() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let state_dir = temp_dir.path().to_path_buf();

        let project = Project {
            root: PathBuf::from("/tmp/repo"),
            id: "test-proj".to_string(),
            state_dir: state_dir.clone(),
        };

        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(7),
            started: now,
            ended: Some(now),
            model_configured: Some("claude-opus".to_string()),
            model_reported: Some("claude-opus".to_string()),
            session_id: Some("session-123".to_string()),
            exit_reason: "success".to_string(),
            gates: vec![],
            usage: None,
            base_sha: "abc123".to_string(),
            candidate_sha: Some("def456".to_string()),
        };

        write_evidence(&project, &record, "Original context").expect("write evidence");

        // Simulate rebuild by removing and recreating a different part of state
        // (this doesn't affect evidence directories)
        let attempt_dir = state_dir.join("attempts").join("7").join("1");
        let original_context =
            fs::read_to_string(attempt_dir.join("context.md")).expect("read original context");

        // Delete everything in state_dir except attempts (simulating rebuild)
        for entry in fs::read_dir(&state_dir).expect("read state dir") {
            let entry = entry.expect("read entry");
            let path = entry.path();
            if path.file_name().unwrap() != "attempts" {
                if path.is_dir() {
                    fs::remove_dir_all(&path).ok();
                } else {
                    fs::remove_file(&path).ok();
                }
            }
        }

        // Evidence should still be accessible
        let records = read_evidence(&project, TaskId::new(7)).expect("read evidence after rebuild");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, AttemptId::new(1));

        let context_after_rebuild =
            fs::read_to_string(attempt_dir.join("context.md")).expect("read context after rebuild");
        assert_eq!(original_context, context_after_rebuild);
    }
}

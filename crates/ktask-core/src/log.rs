//! Structured logging to disk with automatic redaction.
//!
//! Writes newline-delimited JSON records to `<state_dir>/logs/run-<date>.jsonl`,
//! created with restrictive permissions (mode 0600). All content is redacted
//! before being written.

use crate::{AttemptId, Event, EventKind, Phase, Result, TaskId, redact};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use time::OffsetDateTime;

/// Log level for filtering messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    /// Debug level
    Debug,
    /// Info level
    Info,
    /// Warning level
    Warn,
    /// Error level
    Error,
}

/// A structured logging record written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// ISO 8601 timestamp
    pub ts: String,
    /// Log level
    pub level: Level,
    /// Task identifier, None for global messages
    pub task_id: Option<String>,
    /// Attempt number, None if not in an attempt
    pub attempt: Option<u32>,
    /// Current phase, None if not applicable
    pub phase: Option<String>,
    /// Redacted message
    pub message: String,
}

/// Structured logger that writes to a per-run JSONL file.
#[derive(Debug)]
pub struct Logger {
    file: File,
    current_level: Level,
}

impl Logger {
    /// Create a new logger for a specific state directory.
    ///
    /// Creates `<state_dir>/logs/run-<YYYY-MM-DD>.jsonl` if it doesn't exist,
    /// with file mode 0600 (rw-------).
    ///
    /// # Errors
    ///
    /// Returns an error if the logs directory cannot be created or if the
    /// file cannot be opened.
    pub fn new(state_dir: &Path, level: Level) -> Result<Self> {
        let logs_dir = state_dir.join("logs");
        fs::create_dir_all(&logs_dir)?;

        let now = OffsetDateTime::now_utc();
        let date_str = now
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .map_err(|e| crate::Error::Config {
                key: "date_format".to_string(),
                detail: format!("Failed to format date: {e}"),
            })?;

        let log_file = logs_dir.join(format!("run-{date_str}.jsonl"));

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)?;

        // Set restrictive permissions (0600 = rw-------)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&log_file, permissions)?;
        }

        Ok(Logger {
            file,
            current_level: level,
        })
    }

    /// Log a message at the specified level.
    ///
    /// The message is redacted using the global redaction patterns before
    /// being written. Messages are filtered based on the current log level:
    /// only messages at or above the current level are logged.
    ///
    /// # Errors
    ///
    /// Returns an error if the timestamp cannot be formatted, the message
    /// cannot be serialized to JSON, or the write to the file fails.
    pub fn log(
        &mut self,
        level: Level,
        task_id: Option<TaskId>,
        attempt: Option<AttemptId>,
        phase: Option<Phase>,
        message: &str,
    ) -> Result<()> {
        if level < self.current_level {
            return Ok(());
        }

        let now = OffsetDateTime::now_utc();
        let ts = now
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| crate::Error::Config {
                key: "timestamp_format".to_string(),
                detail: format!("Failed to format timestamp: {e}"),
            })?;

        let redacted = redact::redact(message, &[]);

        let record = LogRecord {
            ts,
            level,
            task_id: task_id.map(|id| id.to_string()),
            attempt: attempt.map(AttemptId::get),
            phase: phase.map(|p| format!("{p:?}")),
            message: redacted,
        };

        let json_line = serde_json::to_string(&record).map_err(|e| crate::Error::Config {
            key: "json_serialization".to_string(),
            detail: e.to_string(),
        })?;

        writeln!(self.file, "{json_line}")?;
        self.file.flush()?;

        Ok(())
    }

    /// Log an event from the event bus.
    ///
    /// Extracts relevant information from the event and logs it appropriately.
    ///
    /// # Errors
    ///
    /// Returns an error if the event cannot be logged (same as `log`).
    pub fn log_event(&mut self, event: &Event) -> Result<()> {
        let level = match &event.kind {
            EventKind::PreflightFailed { .. }
            | EventKind::VerifyFailed { .. }
            | EventKind::TaskFailed { .. } => Level::Error,
            _ => Level::Info,
        };

        let (attempt, phase) = extract_attempt_and_phase(&event.kind);

        let message = describe_event(&event.kind);

        self.log(level, event.task_id, attempt, phase, &message)
    }

    /// Set the current log level filter.
    pub fn set_level(&mut self, level: Level) {
        self.current_level = level;
    }
}

/// Extract attempt ID and phase from an event.
fn extract_attempt_and_phase(kind: &EventKind) -> (Option<AttemptId>, Option<Phase>) {
    match kind {
        EventKind::PhaseEntered { attempt, phase } => (Some(*attempt), Some(*phase)),
        EventKind::AttemptStarted { attempt, .. }
        | EventKind::AgentOutput { attempt, .. }
        | EventKind::VerifyPassed { attempt }
        | EventKind::VerifyFailed { attempt, .. }
        | EventKind::PublishStarted { attempt, .. } => (Some(*attempt), None),
        _ => (None, None),
    }
}

/// Create a human-readable description of an event.
fn describe_event(kind: &EventKind) -> String {
    match kind {
        EventKind::TaskQueued { title } => format!("Task queued: {title}"),
        EventKind::PreflightStarted => "Preflight checks started".to_string(),
        EventKind::PreflightPassed { base_sha } => {
            format!("Preflight checks passed (base: {base_sha})")
        }
        EventKind::PreflightFailed { class, detail } => {
            format!("Preflight checks failed: {class:?}: {detail}")
        }
        EventKind::AttemptStarted {
            attempt, protocol, ..
        } => {
            format!("Attempt {} started ({} protocol)", attempt.get(), protocol)
        }
        EventKind::PhaseEntered { attempt, phase } => {
            format!("Attempt {} entered phase {:?}", attempt.get(), phase)
        }
        EventKind::AgentOutput {
            attempt,
            stream,
            text,
        } => {
            format!("Attempt {} {:?}: {}", attempt.get(), stream, text)
        }
        EventKind::VerifyPassed { attempt } => {
            format!("Attempt {} verification passed", attempt.get())
        }
        EventKind::VerifyFailed {
            attempt,
            class,
            detail,
        } => {
            format!(
                "Attempt {} verification failed: {:?}: {}",
                attempt.get(),
                class,
                detail
            )
        }
        EventKind::PublishStarted {
            attempt,
            candidate_sha,
        } => {
            format!(
                "Attempt {} publish started (candidate: {})",
                attempt.get(),
                candidate_sha
            )
        }
        EventKind::PublishVerified { commit, remote_sha } => {
            format!("Publish verified (local: {commit} remote: {remote_sha})")
        }
        EventKind::TaskDone { commit } => format!("Task completed (commit: {commit})"),
        EventKind::TaskFailed { class, detail } => {
            format!("Task failed: {class:?}: {detail}")
        }
        EventKind::TaskCancelled { reason } => format!("Task cancelled: {reason}"),
        EventKind::Paused { reason } => format!("Task paused: {reason:?}"),
        EventKind::Resumed => "Task resumed".to_string(),
        EventKind::Interrupted { phase } => format!("Task interrupted at {phase:?}"),
        EventKind::RecoveryDecision { decision, detail } => {
            format!("Recovery decision: {decision:?}: {detail}")
        }
        EventKind::AttemptRecorded { .. } => "Attempt recorded".to_string(),
        EventKind::GateAcknowledged { by, .. } => {
            format!("Gate acknowledged by {by}")
        }
        EventKind::TddExceptionUsed { exception, reason } => {
            format!("TDD exception used: {exception:?}: {reason}")
        }
        EventKind::GateStarted {
            attempt,
            gate_kind,
            tree_hash,
        } => {
            format!(
                "Attempt {} gate {:?} started (tree: {})",
                attempt.get(),
                gate_kind,
                tree_hash
            )
        }
        EventKind::GateFinished {
            attempt,
            gate_kind,
            passed,
            tree_hash,
            ..
        } => {
            format!(
                "Attempt {} gate {:?} finished (passed: {}, tree: {})",
                attempt.get(),
                gate_kind,
                passed,
                tree_hash
            )
        }
        EventKind::DecisionRaised { request } => {
            format!("Decision raised: {}", request.question)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn get_log_file_path(state_dir: &Path) -> PathBuf {
        let date_str = OffsetDateTime::now_utc()
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .unwrap();
        state_dir.join("logs").join(format!("run-{date_str}.jsonl"))
    }

    #[test]
    fn log_writes_json_record() {
        let temp = TempDir::new().unwrap();
        let mut logger = Logger::new(temp.path(), Level::Debug).unwrap();

        logger
            .log(
                Level::Info,
                None,
                None,
                None,
                "Test message with secret sk-test1234567890",
            )
            .unwrap();

        let log_file = fs::read_to_string(get_log_file_path(temp.path())).unwrap();

        assert!(!log_file.is_empty());
        let record: LogRecord = serde_json::from_str(log_file.trim()).unwrap();
        assert_eq!(record.level, Level::Info);
        assert!(record.message.contains("[redacted]"));
        assert!(!record.message.contains("sk-"));
    }

    #[test]
    fn log_respects_level_filtering() {
        let temp = TempDir::new().unwrap();
        let mut logger = Logger::new(temp.path(), Level::Error).unwrap();

        logger
            .log(Level::Debug, None, None, None, "Debug message")
            .ok();
        logger
            .log(Level::Info, None, None, None, "Info message")
            .ok();
        logger
            .log(Level::Error, None, None, None, "Error message")
            .ok();

        let log_file = fs::read_to_string(get_log_file_path(temp.path())).unwrap();

        let lines: Vec<&str> = log_file.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: LogRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(record.message, "Error message");
    }

    #[test]
    fn log_includes_task_attempt_and_phase() {
        let temp = TempDir::new().unwrap();
        let mut logger = Logger::new(temp.path(), Level::Debug).unwrap();

        let task_id = TaskId::new(1);
        let attempt_id = AttemptId::new(2);
        let phase = Phase::Implement;

        logger
            .log(
                Level::Info,
                Some(task_id),
                Some(attempt_id),
                Some(phase),
                "Message with context",
            )
            .unwrap();

        let log_file = fs::read_to_string(get_log_file_path(temp.path())).unwrap();

        let record: LogRecord = serde_json::from_str(log_file.trim()).unwrap();
        assert_eq!(record.task_id, Some("1".to_string()));
        assert_eq!(record.attempt, Some(2));
        assert_eq!(record.phase, Some("Implement".to_string()));
    }

    #[test]
    fn every_log_line_parses_as_json() {
        let temp = TempDir::new().unwrap();
        let mut logger = Logger::new(temp.path(), Level::Debug).unwrap();

        logger
            .log(Level::Info, None, None, None, "Message 1")
            .unwrap();
        logger
            .log(Level::Warn, None, None, None, "Message 2")
            .unwrap();
        logger
            .log(Level::Error, None, None, None, "Message 3")
            .unwrap();

        let log_file = fs::read_to_string(get_log_file_path(temp.path())).unwrap();

        for line in log_file.lines() {
            if !line.is_empty() {
                let _record: LogRecord =
                    serde_json::from_str(line).expect("Each line must parse as JSON");
            }
        }
    }

    #[test]
    fn planted_secrets_redacted_in_log() {
        let temp = TempDir::new().unwrap();
        let mut logger = Logger::new(temp.path(), Level::Debug).unwrap();

        let secrets = vec![
            "Bearer my-secret-token-123",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnopqrstuvwxyz",
            "sk-proj-test1234567890",
        ];

        for secret in secrets {
            logger
                .log(Level::Info, None, None, None, &format!("Secret: {secret}"))
                .unwrap();
        }

        let log_file = fs::read_to_string(get_log_file_path(temp.path())).unwrap();

        for line in log_file.lines() {
            if !line.is_empty() {
                let record: LogRecord = serde_json::from_str(line).unwrap();
                assert!(!record.message.contains("my-secret"));
                assert!(!record.message.contains("AKIA"));
                assert!(!record.message.contains("ghp_"));
                assert!(!record.message.contains("sk-proj"));
                assert!(record.message.contains("[redacted]"));
            }
        }
    }
}

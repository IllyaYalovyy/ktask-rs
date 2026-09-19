//! Attempt records: complete evidence of every attempt at a task.

use crate::{AttemptId, GateResult, TaskId, Usage};
use serde::{Deserialize, Serialize};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AttemptId, TaskId};
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
}

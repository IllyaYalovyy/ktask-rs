//! Event kinds and their payloads.

use crate::{
    AttemptId, AttemptRecord, EventSeq, FailureClass, PauseReason, Phase, Recovery, Stream, TaskId,
    TddException,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// An event in the journal representing a state change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EventKind {
    /// Task was added to the queue.
    TaskQueued {
        /// Task title.
        title: String,
    },
    /// Preflight checks started.
    PreflightStarted,
    /// Preflight checks passed.
    PreflightPassed {
        /// Base commit SHA before changes.
        base_sha: String,
    },
    /// Preflight checks failed.
    PreflightFailed {
        /// Failure classification.
        class: FailureClass,
        /// Failure details.
        detail: String,
    },
    /// An attempt started.
    AttemptStarted {
        /// Attempt identifier.
        attempt: AttemptId,
        /// Protocol used for this attempt.
        protocol: String,
        /// Process ID of the agent.
        pid: u32,
        /// Base commit SHA.
        base_sha: String,
    },
    /// Agent entered a new phase.
    PhaseEntered {
        /// Attempt identifier.
        attempt: AttemptId,
        /// Phase entered.
        phase: Phase,
    },
    /// Agent produced output.
    AgentOutput {
        /// Attempt identifier.
        attempt: AttemptId,
        /// Output stream type.
        stream: Stream,
        /// Output text.
        text: String,
    },
    /// Verification passed.
    VerifyPassed {
        /// Attempt identifier.
        attempt: AttemptId,
    },
    /// Verification failed.
    VerifyFailed {
        /// Attempt identifier.
        attempt: AttemptId,
        /// Failure classification.
        class: FailureClass,
        /// Failure details.
        detail: String,
    },
    /// Publishing started.
    PublishStarted {
        /// Attempt identifier.
        attempt: AttemptId,
        /// Candidate commit SHA.
        candidate_sha: String,
    },
    /// Publish was verified.
    PublishVerified {
        /// Published commit SHA.
        commit: String,
        /// Remote SHA after push.
        remote_sha: String,
    },
    /// Task completed successfully.
    TaskDone {
        /// Final commit SHA.
        commit: String,
    },
    /// Task failed.
    TaskFailed {
        /// Failure classification.
        class: FailureClass,
        /// Failure details.
        detail: String,
    },
    /// Task was cancelled.
    TaskCancelled {
        /// Cancellation reason.
        reason: String,
    },
    /// Task was paused.
    Paused {
        /// Pause reason.
        reason: PauseReason,
    },
    /// Task resumed.
    Resumed,
    /// Task was interrupted.
    Interrupted {
        /// Phase at interruption.
        phase: Phase,
    },
    /// Recovery decision was made.
    RecoveryDecision {
        /// Recovery decision.
        decision: Recovery,
        /// Decision details.
        detail: String,
    },
    /// Task acknowledged after completion.
    GateAcknowledged {
        /// User who acknowledged.
        by: String,
        /// Acknowledgment timestamp.
        at: OffsetDateTime,
    },
    /// Attempt record was persisted.
    AttemptRecorded {
        /// The attempt record.
        record: Box<AttemptRecord>,
    },
    /// TDD exception was used to skip the red phase.
    TddExceptionUsed {
        /// The exception type.
        exception: TddException,
        /// Reason for using the exception.
        reason: String,
    },
}

impl EventKind {
    /// Return the discriminant name as a string.
    #[must_use]
    pub fn discriminant(&self) -> &'static str {
        match self {
            EventKind::TaskQueued { .. } => "TaskQueued",
            EventKind::PreflightStarted => "PreflightStarted",
            EventKind::PreflightPassed { .. } => "PreflightPassed",
            EventKind::PreflightFailed { .. } => "PreflightFailed",
            EventKind::AttemptStarted { .. } => "AttemptStarted",
            EventKind::PhaseEntered { .. } => "PhaseEntered",
            EventKind::AgentOutput { .. } => "AgentOutput",
            EventKind::VerifyPassed { .. } => "VerifyPassed",
            EventKind::VerifyFailed { .. } => "VerifyFailed",
            EventKind::PublishStarted { .. } => "PublishStarted",
            EventKind::PublishVerified { .. } => "PublishVerified",
            EventKind::TaskDone { .. } => "TaskDone",
            EventKind::TaskFailed { .. } => "TaskFailed",
            EventKind::TaskCancelled { .. } => "TaskCancelled",
            EventKind::Paused { .. } => "Paused",
            EventKind::Resumed => "Resumed",
            EventKind::Interrupted { .. } => "Interrupted",
            EventKind::RecoveryDecision { .. } => "RecoveryDecision",
            EventKind::GateAcknowledged { .. } => "GateAcknowledged",
            EventKind::AttemptRecorded { .. } => "AttemptRecorded",
            EventKind::TddExceptionUsed { .. } => "TddExceptionUsed",
        }
    }
}

/// A stored event carrying sequence, timestamp, task and payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic global event sequence number.
    pub seq: EventSeq,
    /// Event timestamp in RFC 3339 format (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    /// Task identifier, None for global events.
    pub task_id: Option<TaskId>,
    /// Event kind and payload.
    pub kind: EventKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AttemptRecord;

    #[test]
    fn event_kind_task_queued_roundtrips_through_json() {
        let event = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_preflight_started_roundtrips_through_json() {
        let event = EventKind::PreflightStarted;
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_preflight_passed_roundtrips_through_json() {
        let event = EventKind::PreflightPassed {
            base_sha: "abc123".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_preflight_failed_roundtrips_through_json() {
        let event = EventKind::PreflightFailed {
            class: FailureClass::EnvironmentFailure,
            detail: "Missing dependency".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_attempt_started_roundtrips_through_json() {
        let event = EventKind::AttemptStarted {
            attempt: AttemptId::new(1),
            protocol: "direct".to_string(),
            pid: 1234,
            base_sha: "def456".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_phase_entered_roundtrips_through_json() {
        let event = EventKind::PhaseEntered {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_agent_output_roundtrips_through_json() {
        let event = EventKind::AgentOutput {
            attempt: AttemptId::new(1),
            stream: Stream::Stdout,
            text: "Output text".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_verify_passed_roundtrips_through_json() {
        let event = EventKind::VerifyPassed {
            attempt: AttemptId::new(1),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_verify_failed_roundtrips_through_json() {
        let event = EventKind::VerifyFailed {
            attempt: AttemptId::new(1),
            class: FailureClass::VerificationFailure,
            detail: "Verification failed".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_publish_started_roundtrips_through_json() {
        let event = EventKind::PublishStarted {
            attempt: AttemptId::new(1),
            candidate_sha: "ghi789".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_publish_verified_roundtrips_through_json() {
        let event = EventKind::PublishVerified {
            commit: "abc123commit".to_string(),
            remote_sha: "remote123".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_task_done_roundtrips_through_json() {
        let event = EventKind::TaskDone {
            commit: "final123".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_task_failed_roundtrips_through_json() {
        let event = EventKind::TaskFailed {
            class: FailureClass::AgentFailure,
            detail: "Agent crashed".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_task_cancelled_roundtrips_through_json() {
        let event = EventKind::TaskCancelled {
            reason: "User cancelled".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_paused_roundtrips_through_json() {
        let event = EventKind::Paused {
            reason: PauseReason::Input,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_resumed_roundtrips_through_json() {
        let event = EventKind::Resumed;
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_interrupted_roundtrips_through_json() {
        let event = EventKind::Interrupted {
            phase: Phase::Implement,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_recovery_decision_roundtrips_through_json() {
        let event = EventKind::RecoveryDecision {
            decision: Recovery::Resume,
            detail: "Resuming task".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_gate_acknowledged_roundtrips_through_json() {
        let dt = OffsetDateTime::now_utc();
        let event = EventKind::GateAcknowledged {
            by: "user@example.com".to_string(),
            at: dt,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_discriminant_preflight_variants() {
        assert_eq!(
            EventKind::TaskQueued {
                title: "Test".to_string()
            }
            .discriminant(),
            "TaskQueued"
        );
        assert_eq!(
            EventKind::PreflightStarted.discriminant(),
            "PreflightStarted"
        );
        assert_eq!(
            EventKind::PreflightPassed {
                base_sha: "abc".to_string()
            }
            .discriminant(),
            "PreflightPassed"
        );
        assert_eq!(
            EventKind::PreflightFailed {
                class: FailureClass::AgentFailure,
                detail: "fail".to_string()
            }
            .discriminant(),
            "PreflightFailed"
        );
    }

    #[test]
    fn event_kind_discriminant_attempt_variants() {
        assert_eq!(
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: 100,
                base_sha: "abc".to_string()
            }
            .discriminant(),
            "AttemptStarted"
        );
        assert_eq!(
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Goal
            }
            .discriminant(),
            "PhaseEntered"
        );
        assert_eq!(
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "output".to_string()
            }
            .discriminant(),
            "AgentOutput"
        );
    }

    #[test]
    fn event_kind_discriminant_verification_variants() {
        assert_eq!(
            EventKind::VerifyPassed {
                attempt: AttemptId::new(1)
            }
            .discriminant(),
            "VerifyPassed"
        );
        assert_eq!(
            EventKind::VerifyFailed {
                attempt: AttemptId::new(1),
                class: FailureClass::VerificationFailure,
                detail: "fail".to_string()
            }
            .discriminant(),
            "VerifyFailed"
        );
    }

    #[test]
    fn event_kind_discriminant_publish_variants() {
        assert_eq!(
            EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "abc".to_string()
            }
            .discriminant(),
            "PublishStarted"
        );
        assert_eq!(
            EventKind::PublishVerified {
                commit: "abc".to_string(),
                remote_sha: "def".to_string()
            }
            .discriminant(),
            "PublishVerified"
        );
    }

    #[test]
    fn event_kind_discriminant_completion_variants() {
        assert_eq!(
            EventKind::TaskDone {
                commit: "abc".to_string()
            }
            .discriminant(),
            "TaskDone"
        );
        assert_eq!(
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "fail".to_string()
            }
            .discriminant(),
            "TaskFailed"
        );
        assert_eq!(
            EventKind::TaskCancelled {
                reason: "cancelled".to_string()
            }
            .discriminant(),
            "TaskCancelled"
        );
    }

    #[test]
    fn event_kind_discriminant_state_variants() {
        assert_eq!(
            EventKind::Paused {
                reason: PauseReason::Input
            }
            .discriminant(),
            "Paused"
        );
        assert_eq!(EventKind::Resumed.discriminant(), "Resumed");
        assert_eq!(
            EventKind::Interrupted { phase: Phase::Goal }.discriminant(),
            "Interrupted"
        );
    }

    #[test]
    fn event_kind_discriminant_recovery_and_gate_variants() {
        assert_eq!(
            EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "detail".to_string()
            }
            .discriminant(),
            "RecoveryDecision"
        );
        assert_eq!(
            EventKind::GateAcknowledged {
                by: "user".to_string(),
                at: OffsetDateTime::now_utc()
            }
            .discriminant(),
            "GateAcknowledged"
        );
    }

    #[test]
    fn event_serializes_with_rfc3339_timestamp() {
        let ts = OffsetDateTime::now_utc();
        let event = Event {
            seq: EventSeq::new(42),
            ts,
            task_id: Some(TaskId::new(1)),
            kind: EventKind::TaskQueued {
                title: "Test task".to_string(),
            },
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");

        // Verify the timestamp is in RFC 3339 format
        let ts_str = parsed["ts"].as_str().expect("timestamp should be a string");
        assert!(
            ts_str.contains('T'),
            "RFC 3339 format requires 'T' separator"
        );
        assert!(
            ts_str.contains('Z') || ts_str.contains('+') || ts_str.contains('-'),
            "RFC 3339 format requires timezone info"
        );
    }

    #[test]
    fn event_roundtrips_through_json() {
        let ts = OffsetDateTime::now_utc();
        let original = Event {
            seq: EventSeq::new(42),
            ts,
            task_id: Some(TaskId::new(1)),
            kind: EventKind::TaskQueued {
                title: "Test task".to_string(),
            },
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: Event = serde_json::from_str(&json).expect("deserialize");

        // Verify that the round-trip preserves the same instant
        assert_eq!(original.seq, deserialized.seq);
        assert_eq!(original.task_id, deserialized.task_id);
        assert_eq!(original.kind, deserialized.kind);
        // OffsetDateTime equality checks the same instant, which is what we need
        assert_eq!(original.ts, deserialized.ts);
    }

    #[test]
    fn event_with_none_task_id() {
        let ts = OffsetDateTime::now_utc();
        let event = Event {
            seq: EventSeq::new(1),
            ts,
            task_id: None,
            kind: EventKind::PreflightStarted,
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: Event = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_attempt_recorded_roundtrips_through_json() {
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
        let event = EventKind::AttemptRecorded {
            record: Box::new(record),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deserialized);
    }

    #[test]
    fn event_kind_discriminant_attempt_recorded() {
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
        assert_eq!(
            EventKind::AttemptRecorded {
                record: Box::new(record)
            }
            .discriminant(),
            "AttemptRecorded"
        );
    }
}

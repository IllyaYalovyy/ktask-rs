//! Task state transitions and phases.

use crate::classify::FailureClass;
use crate::ids::AttemptId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Lifecycle state of a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskState {
    /// Task has been queued but not yet started.
    Queued,
    /// Running preflight checks.
    Preflight,
    /// Task is running.
    Running {
        /// The current attempt number.
        attempt: AttemptId,
        /// The current phase of execution.
        phase: Phase,
    },
    /// Task is being remediated after a failure.
    Remediating {
        /// The current attempt number.
        attempt: AttemptId,
        /// The current phase of remediation.
        phase: Phase,
    },
    /// Task output is being verified.
    Verifying {
        /// The current attempt number.
        attempt: AttemptId,
    },
    /// Task changes are being published.
    Publishing {
        /// The current attempt number.
        attempt: AttemptId,
    },
    /// Task has been published and verified.
    PublishedVerified {
        /// The published commit hash.
        commit: String,
    },
    /// Task completed successfully.
    Done,
    /// Verification gate has been acknowledged.
    Acknowledged {
        /// User who acknowledged the gate.
        by: String,
        /// When the gate was acknowledged.
        at: OffsetDateTime,
    },
    /// Task is paused, can resume to a specific state.
    Paused {
        /// The reason the task was paused.
        reason: PauseReason,
        /// The state to resume to when unpaused.
        resume_to: Box<TaskState>,
    },
    /// Task failed with classification.
    Failed {
        /// The failure classification.
        class: FailureClass,
        /// Details about the failure.
        detail: String,
    },
    /// Task was cancelled.
    Cancelled,
}

impl TaskState {
    /// Check if this state is a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Done
                | TaskState::PublishedVerified { .. }
                | TaskState::Failed { .. }
                | TaskState::Cancelled
        )
    }

    /// Check if this state is paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self, TaskState::Paused { .. })
    }

    /// Get the name of this state variant.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            TaskState::Queued => "Queued",
            TaskState::Preflight => "Preflight",
            TaskState::Running { .. } => "Running",
            TaskState::Remediating { .. } => "Remediating",
            TaskState::Verifying { .. } => "Verifying",
            TaskState::Publishing { .. } => "Publishing",
            TaskState::PublishedVerified { .. } => "PublishedVerified",
            TaskState::Done => "Done",
            TaskState::Acknowledged { .. } => "Acknowledged",
            TaskState::Paused { .. } => "Paused",
            TaskState::Failed { .. } => "Failed",
            TaskState::Cancelled => "Cancelled",
        }
    }
}

/// Phases of task execution, from planning through publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Goal definition.
    Goal,
    /// Scope clarification.
    Scope,
    /// Acceptance tests.
    AcceptanceTests,
    /// Implementation.
    Implement,
    /// Red phase (test fails).
    Red,
    /// Green phase (test passes).
    Green,
    /// Refactoring.
    Refactor,
    /// Code review.
    Review,
    /// Hardening and edge cases.
    Harden,
    /// Final checks.
    DoneCheck,
    /// Verification phase.
    Verify,
    /// Publication phase.
    Publish,
}

/// Reasons a task can be paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// Paused due to rate limit.
    Limit {
        /// Time until the limit expires, or None if duration is unknown.
        until: Option<OffsetDateTime>,
    },
    /// Paused waiting for input.
    Input,
    /// Paused at a human gate.
    HumanGate,
    /// Paused due to interruption.
    Interrupted,
    /// Paused due to blocking condition.
    Blocked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::FailureClass;
    use crate::ids::AttemptId;

    #[test]
    fn task_state_queued_roundtrips_through_json() {
        let state = TaskState::Queued;
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_preflight_roundtrips_through_json() {
        let state = TaskState::Preflight;
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_running_roundtrips_through_json() {
        let state = TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_remediating_roundtrips_through_json() {
        let state = TaskState::Remediating {
            attempt: AttemptId::new(2),
            phase: Phase::Green,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_verifying_roundtrips_through_json() {
        let state = TaskState::Verifying {
            attempt: AttemptId::new(1),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_publishing_roundtrips_through_json() {
        let state = TaskState::Publishing {
            attempt: AttemptId::new(1),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_published_verified_roundtrips_through_json() {
        let state = TaskState::PublishedVerified {
            commit: "abc123def456".to_string(),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_done_roundtrips_through_json() {
        let state = TaskState::Done;
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_acknowledged_roundtrips_through_json() {
        let now = OffsetDateTime::now_utc();
        let state = TaskState::Acknowledged {
            by: "user@example.com".to_string(),
            at: now,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_paused_with_boxed_resume_roundtrips_through_json() {
        let resume_to = Box::new(TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        });
        let state = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_paused_with_limit_roundtrips_through_json() {
        let resume_to = Box::new(TaskState::Queued);
        let state = TaskState::Paused {
            reason: PauseReason::Limit { until: None },
            resume_to,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_failed_roundtrips_through_json() {
        let state = TaskState::Failed {
            class: FailureClass::AgentFailure,
            detail: "agent crashed".to_string(),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn task_state_cancelled_roundtrips_through_json() {
        let state = TaskState::Cancelled;
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, deserialized);
    }

    #[test]
    fn is_terminal_returns_true_for_terminal_states() {
        assert!(TaskState::Done.is_terminal());
        assert!(
            TaskState::PublishedVerified {
                commit: "abc123".to_string()
            }
            .is_terminal()
        );
        assert!(
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "failed".to_string()
            }
            .is_terminal()
        );
        assert!(TaskState::Cancelled.is_terminal());
    }

    #[test]
    fn is_terminal_returns_false_for_non_terminal_states() {
        assert!(!TaskState::Queued.is_terminal());
        assert!(!TaskState::Preflight.is_terminal());
        assert!(
            !TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement
            }
            .is_terminal()
        );
        assert!(
            !TaskState::Remediating {
                attempt: AttemptId::new(1),
                phase: Phase::Green
            }
            .is_terminal()
        );
        assert!(
            !TaskState::Verifying {
                attempt: AttemptId::new(1)
            }
            .is_terminal()
        );
        assert!(
            !TaskState::Publishing {
                attempt: AttemptId::new(1)
            }
            .is_terminal()
        );
        let now = OffsetDateTime::now_utc();
        assert!(
            !TaskState::Acknowledged {
                by: "user".to_string(),
                at: now
            }
            .is_terminal()
        );
        assert!(
            !TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Queued)
            }
            .is_terminal()
        );
    }

    #[test]
    fn is_paused_returns_true_for_paused_state() {
        assert!(
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Queued)
            }
            .is_paused()
        );
    }

    #[test]
    fn is_paused_returns_false_for_non_paused_states() {
        assert!(!TaskState::Queued.is_paused());
        assert!(!TaskState::Preflight.is_paused());
        assert!(
            !TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement
            }
            .is_paused()
        );
        assert!(!TaskState::Done.is_paused());
    }

    #[test]
    fn name_returns_correct_variant_names() {
        assert_eq!(TaskState::Queued.name(), "Queued");
        assert_eq!(TaskState::Preflight.name(), "Preflight");
        assert_eq!(
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement
            }
            .name(),
            "Running"
        );
        assert_eq!(
            TaskState::Remediating {
                attempt: AttemptId::new(1),
                phase: Phase::Green
            }
            .name(),
            "Remediating"
        );
        assert_eq!(
            TaskState::Verifying {
                attempt: AttemptId::new(1)
            }
            .name(),
            "Verifying"
        );
        assert_eq!(
            TaskState::Publishing {
                attempt: AttemptId::new(1)
            }
            .name(),
            "Publishing"
        );
        assert_eq!(
            TaskState::PublishedVerified {
                commit: "abc123".to_string()
            }
            .name(),
            "PublishedVerified"
        );
        assert_eq!(TaskState::Done.name(), "Done");
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            TaskState::Acknowledged {
                by: "user".to_string(),
                at: now
            }
            .name(),
            "Acknowledged"
        );
        assert_eq!(
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Queued)
            }
            .name(),
            "Paused"
        );
        assert_eq!(
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "failed".to_string()
            }
            .name(),
            "Failed"
        );
        assert_eq!(TaskState::Cancelled.name(), "Cancelled");
    }

    #[test]
    fn phase_has_twelve_variants() {
        let variants = [
            Phase::Goal,
            Phase::Scope,
            Phase::AcceptanceTests,
            Phase::Implement,
            Phase::Red,
            Phase::Green,
            Phase::Refactor,
            Phase::Review,
            Phase::Harden,
            Phase::DoneCheck,
            Phase::Verify,
            Phase::Publish,
        ];
        assert_eq!(variants.len(), 12);
    }

    #[test]
    fn phase_roundtrips_through_json() {
        for phase in [
            Phase::Goal,
            Phase::Scope,
            Phase::AcceptanceTests,
            Phase::Implement,
            Phase::Red,
            Phase::Green,
            Phase::Refactor,
            Phase::Review,
            Phase::Harden,
            Phase::DoneCheck,
            Phase::Verify,
            Phase::Publish,
        ] {
            let json = serde_json::to_string(&phase).expect("serialize phase");
            let deserialized: Phase = serde_json::from_str(&json).expect("deserialize phase");
            assert_eq!(phase, deserialized);
        }
    }

    #[test]
    fn pause_reason_has_five_variants() {
        let variants = [
            PauseReason::Limit { until: None },
            PauseReason::Input,
            PauseReason::HumanGate,
            PauseReason::Interrupted,
            PauseReason::Blocked,
        ];
        assert_eq!(variants.len(), 5);
    }

    #[test]
    fn pause_reason_roundtrips_through_json() {
        let variants = vec![
            PauseReason::Limit { until: None },
            PauseReason::Input,
            PauseReason::HumanGate,
            PauseReason::Interrupted,
            PauseReason::Blocked,
        ];
        for reason in variants {
            let json = serde_json::to_string(&reason).expect("serialize reason");
            let deserialized: PauseReason =
                serde_json::from_str(&json).expect("deserialize reason");
            assert_eq!(reason, deserialized);
        }
    }
}

//! Task state transitions and phases.

use crate::classify::FailureClass;
use crate::error::{Error, Result};
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

/// Apply an event to the current state to produce the next state.
///
/// This is the single dispatcher for all state transitions. Each state delegates to
/// a private helper function that exhaustively matches all event types, with illegal
/// transitions collected into a single error arm.
///
/// # Errors
///
/// Returns `Error::InvalidTransition` if the event is not valid in the current state.
pub fn apply(state: &TaskState, event: &crate::event::EventKind) -> Result<TaskState> {
    match state {
        TaskState::Queued => from_queued(event),
        TaskState::Preflight => from_preflight(event),
        TaskState::Running { attempt, phase } => from_running(*attempt, *phase, event),
        TaskState::Remediating { attempt, phase } => from_remediating(*attempt, *phase, event),
        TaskState::Verifying { attempt } => from_verifying(*attempt, event),
        TaskState::Publishing { attempt } => from_publishing(*attempt, event),
        TaskState::PublishedVerified { commit } => from_published_verified(commit, event),
        TaskState::Paused { reason, resume_to } => from_paused(reason, resume_to, event),
        TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled => Err(Error::InvalidTransition {
            from: state.name().to_string(),
            event: crate::event::EventKind::discriminant(event).to_string(),
        }),
    }
}

fn from_queued(event: &crate::event::EventKind) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::TaskQueued { .. } => Ok(TaskState::Queued),
        EventKind::PreflightStarted => Ok(TaskState::Preflight),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Queued),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: "Queued".to_string(),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_preflight(event: &crate::event::EventKind) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::PreflightStarted | EventKind::PreflightPassed { .. } => Ok(TaskState::Preflight),
        EventKind::PreflightFailed { class, detail } => Ok(TaskState::Failed {
            class: *class,
            detail: detail.clone(),
        }),
        EventKind::AttemptStarted { attempt, .. } => Ok(TaskState::Running {
            attempt: *attempt,
            phase: Phase::Goal,
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Preflight),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: "Preflight".to_string(),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_running(
    attempt: AttemptId,
    phase: Phase,
    event: &crate::event::EventKind,
) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::PhaseEntered {
            attempt: phase_attempt,
            phase: new_phase,
        } => {
            if *phase_attempt == attempt {
                Ok(TaskState::Running {
                    attempt,
                    phase: *new_phase,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Running({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::AgentOutput {
            attempt: output_attempt,
            ..
        } => {
            if *output_attempt == attempt {
                Ok(TaskState::Running { attempt, phase })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Running({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::VerifyPassed {
            attempt: verify_attempt,
        } => {
            if *verify_attempt == attempt {
                Ok(TaskState::Verifying {
                    attempt: *verify_attempt,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Running({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::VerifyFailed {
            attempt: fail_attempt,
            class: _,
            detail: _,
        } => {
            if *fail_attempt == attempt {
                Ok(TaskState::Remediating {
                    attempt: *fail_attempt,
                    phase: Phase::Red,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Running({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::TaskFailed {
            class: fail_class,
            detail: fail_detail,
        } => Ok(TaskState::Failed {
            class: *fail_class,
            detail: fail_detail.clone(),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Running { attempt, phase }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Running { attempt, phase }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: format!("Running({attempt})"),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_remediating(
    attempt: AttemptId,
    phase: Phase,
    event: &crate::event::EventKind,
) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::PhaseEntered {
            attempt: phase_attempt,
            phase: new_phase,
        } => {
            if *phase_attempt == attempt {
                Ok(TaskState::Remediating {
                    attempt,
                    phase: *new_phase,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Remediating({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::AgentOutput {
            attempt: output_attempt,
            ..
        } => {
            if *output_attempt == attempt {
                Ok(TaskState::Remediating { attempt, phase })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Remediating({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::AttemptStarted {
            attempt: new_attempt,
            ..
        } => {
            if *new_attempt > attempt {
                Ok(TaskState::Running {
                    attempt: *new_attempt,
                    phase: Phase::Goal,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Remediating({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::VerifyPassed {
            attempt: verify_attempt,
        } => {
            if *verify_attempt == attempt {
                Ok(TaskState::Verifying {
                    attempt: *verify_attempt,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Remediating({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::VerifyFailed {
            attempt: fail_attempt,
            class: _,
            detail: _,
        } => {
            if *fail_attempt == attempt {
                Ok(TaskState::Remediating {
                    attempt: *fail_attempt,
                    phase: Phase::Red,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Remediating({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::TaskFailed {
            class: fail_class,
            detail: fail_detail,
        } => Ok(TaskState::Failed {
            class: *fail_class,
            detail: fail_detail.clone(),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Remediating { attempt, phase }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Remediating { attempt, phase }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: format!("Remediating({attempt})"),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_verifying(attempt: AttemptId, event: &crate::event::EventKind) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::PublishStarted {
            attempt: pub_attempt,
            ..
        } => {
            if *pub_attempt == attempt {
                Ok(TaskState::Publishing {
                    attempt: *pub_attempt,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Verifying({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::VerifyFailed {
            attempt: fail_attempt,
            class: _,
            detail: _,
        } => {
            if *fail_attempt == attempt {
                Ok(TaskState::Remediating {
                    attempt: *fail_attempt,
                    phase: Phase::Red,
                })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Verifying({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::TaskFailed {
            class: fail_class,
            detail: fail_detail,
        } => Ok(TaskState::Failed {
            class: *fail_class,
            detail: fail_detail.clone(),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Verifying { attempt }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Verifying { attempt }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: format!("Verifying({attempt})"),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_publishing(attempt: AttemptId, event: &crate::event::EventKind) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::PublishStarted {
            attempt: pub_attempt,
            ..
        } => {
            if *pub_attempt == attempt {
                Ok(TaskState::Publishing { attempt })
            } else {
                Err(Error::InvalidTransition {
                    from: format!("Publishing({attempt})"),
                    event: event.discriminant().to_string(),
                })
            }
        }
        EventKind::PublishVerified { commit, .. } => Ok(TaskState::PublishedVerified {
            commit: commit.clone(),
        }),
        EventKind::TaskFailed {
            class: fail_class,
            detail: fail_detail,
        } => Ok(TaskState::Failed {
            class: *fail_class,
            detail: fail_detail.clone(),
        }),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::Publishing { attempt }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Publishing { attempt }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: format!("Publishing({attempt})"),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_published_verified(commit: &str, event: &crate::event::EventKind) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::TaskDone { commit: _ } => Ok(TaskState::Done),
        EventKind::Paused { reason } => Ok(TaskState::Paused {
            reason: reason.clone(),
            resume_to: Box::new(TaskState::PublishedVerified {
                commit: commit.to_string(),
            }),
        }),
        EventKind::Interrupted { .. } => Ok(TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::PublishedVerified {
                commit: commit.to_string(),
            }),
        }),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: "PublishedVerified".to_string(),
            event: event.discriminant().to_string(),
        }),
    }
}

fn from_paused(
    _reason: &PauseReason,
    resume_to: &TaskState,
    event: &crate::event::EventKind,
) -> Result<TaskState> {
    use crate::event::EventKind;

    match event {
        EventKind::Resumed => Ok(resume_to.clone()),
        EventKind::TaskCancelled { .. } => Ok(TaskState::Cancelled),
        EventKind::Paused { .. }
        | EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::Interrupted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::GateAcknowledged { .. } => Err(Error::InvalidTransition {
            from: "Paused".to_string(),
            event: event.discriminant().to_string(),
        }),
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
    use crate::event::EventKind;
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

    mod state_transitions {
        use super::*;

        #[test]
        fn happy_path_queued_to_done() {
            let attempt = AttemptId::new(1);

            let state = TaskState::Queued;
            let event = EventKind::PreflightStarted;
            let state = apply(&state, &event).expect("preflight started");
            assert_eq!(state, TaskState::Preflight);

            let event = EventKind::PreflightPassed {
                base_sha: "abc123".to_string(),
            };
            let state = apply(&state, &event).expect("preflight passed");
            assert_eq!(state, TaskState::Preflight);

            let event = EventKind::AttemptStarted {
                attempt,
                protocol: "direct".to_string(),
                pid: 1234,
                base_sha: "abc123".to_string(),
            };
            let state = apply(&state, &event).expect("attempt started");
            assert_eq!(
                state,
                TaskState::Running {
                    attempt,
                    phase: Phase::Goal
                }
            );

            let event = EventKind::PhaseEntered {
                attempt,
                phase: Phase::Implement,
            };
            let state = apply(&state, &event).expect("phase entered");
            assert_eq!(
                state,
                TaskState::Running {
                    attempt,
                    phase: Phase::Implement
                }
            );

            let event = EventKind::PhaseEntered {
                attempt,
                phase: Phase::DoneCheck,
            };
            let state = apply(&state, &event).expect("done check phase");
            assert_eq!(
                state,
                TaskState::Running {
                    attempt,
                    phase: Phase::DoneCheck
                }
            );

            let event = EventKind::VerifyPassed { attempt };
            let state = apply(&state, &event).expect("verify passed");
            assert_eq!(state, TaskState::Verifying { attempt });

            let event = EventKind::PublishStarted {
                attempt,
                candidate_sha: "def456".to_string(),
            };
            let state = apply(&state, &event).expect("publish started");
            assert_eq!(state, TaskState::Publishing { attempt });

            let event = EventKind::PublishVerified {
                commit: "ghi789".to_string(),
                remote_sha: "ghi789".to_string(),
            };
            let state = apply(&state, &event).expect("publish verified");
            assert_eq!(
                state,
                TaskState::PublishedVerified {
                    commit: "ghi789".to_string()
                }
            );

            let event = EventKind::TaskDone {
                commit: "ghi789".to_string(),
            };
            let state = apply(&state, &event).expect("task done");
            assert_eq!(state, TaskState::Done);
        }

        #[test]
        fn from_queued_task_queued_is_idempotent() {
            let state = TaskState::Queued;
            let event = EventKind::TaskQueued {
                title: "Test task".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Queued);
        }

        #[test]
        fn from_queued_preflight_started() {
            let state = TaskState::Queued;
            let event = EventKind::PreflightStarted;
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Preflight);
        }

        #[test]
        fn from_queued_illegal_event_verify_passed() {
            let state = TaskState::Queued;
            let event = EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Queued"));
        }

        #[test]
        fn from_preflight_preflight_started_is_idempotent() {
            let state = TaskState::Preflight;
            let event = EventKind::PreflightStarted;
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Preflight);
        }

        #[test]
        fn from_preflight_preflight_passed() {
            let state = TaskState::Preflight;
            let event = EventKind::PreflightPassed {
                base_sha: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Preflight);
        }

        #[test]
        fn from_preflight_preflight_failed() {
            let state = TaskState::Preflight;
            let event = EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "Missing tool".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Failed {
                    class: FailureClass::EnvironmentFailure,
                    detail: "Missing tool".to_string()
                }
            );
        }

        #[test]
        fn from_preflight_attempt_started() {
            let state = TaskState::Preflight;
            let attempt = AttemptId::new(1);
            let event = EventKind::AttemptStarted {
                attempt,
                protocol: "direct".to_string(),
                pid: 1234,
                base_sha: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Running {
                    attempt,
                    phase: Phase::Goal
                }
            );
        }

        #[test]
        fn from_preflight_illegal_event_phase_entered() {
            let state = TaskState::Preflight;
            let event = EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Preflight"));
        }

        #[test]
        fn from_running_phase_entered() {
            let state = TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Goal,
            };
            let event = EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement
                }
            );
        }

        #[test]
        fn from_running_verify_passed() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Running {
                attempt,
                phase: Phase::DoneCheck,
            };
            let event = EventKind::VerifyPassed { attempt };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Verifying { attempt });
        }

        #[test]
        fn from_running_verify_failed_enters_remediation() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Running {
                attempt,
                phase: Phase::DoneCheck,
            };
            let event = EventKind::VerifyFailed {
                attempt,
                class: FailureClass::VerificationFailure,
                detail: "Test failed".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Remediating {
                    attempt,
                    phase: Phase::Red
                }
            );
        }

        #[test]
        fn from_running_illegal_event_publish_started() {
            let state = TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            };
            let event = EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "abc123".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Running"));
        }

        #[test]
        fn from_remediating_new_attempt_starts() {
            let state = TaskState::Remediating {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            };
            let event = EventKind::AttemptStarted {
                attempt: AttemptId::new(2),
                protocol: "direct".to_string(),
                pid: 5678,
                base_sha: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Running {
                    attempt: AttemptId::new(2),
                    phase: Phase::Goal
                }
            );
        }

        #[test]
        fn from_remediating_verify_passed() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Remediating {
                attempt,
                phase: Phase::Green,
            };
            let event = EventKind::VerifyPassed { attempt };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Verifying { attempt });
        }

        #[test]
        fn from_remediating_illegal_event_task_queued() {
            let state = TaskState::Remediating {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            };
            let event = EventKind::TaskQueued {
                title: "Task".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Remediating"));
        }

        #[test]
        fn from_verifying_publish_started() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Verifying { attempt };
            let event = EventKind::PublishStarted {
                attempt,
                candidate_sha: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Publishing { attempt });
        }

        #[test]
        fn from_verifying_verify_failed() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Verifying { attempt };
            let event = EventKind::VerifyFailed {
                attempt,
                class: FailureClass::VerificationFailure,
                detail: "Test failed".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Remediating {
                    attempt,
                    phase: Phase::Red
                }
            );
        }

        #[test]
        fn from_verifying_illegal_event_preflight_started() {
            let state = TaskState::Verifying {
                attempt: AttemptId::new(1),
            };
            let event = EventKind::PreflightStarted;
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Verifying"));
        }

        #[test]
        fn from_publishing_publish_verified() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Publishing { attempt };
            let event = EventKind::PublishVerified {
                commit: "abc123".to_string(),
                remote_sha: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::PublishedVerified {
                    commit: "abc123".to_string()
                }
            );
        }

        #[test]
        fn from_publishing_publish_started_is_idempotent() {
            let attempt = AttemptId::new(1);
            let state = TaskState::Publishing { attempt };
            let event = EventKind::PublishStarted {
                attempt,
                candidate_sha: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Publishing { attempt });
        }

        #[test]
        fn from_publishing_illegal_event_verify_passed() {
            let state = TaskState::Publishing {
                attempt: AttemptId::new(1),
            };
            let event = EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Publishing"));
        }

        #[test]
        fn from_published_verified_task_done() {
            let state = TaskState::PublishedVerified {
                commit: "abc123".to_string(),
            };
            let event = EventKind::TaskDone {
                commit: "abc123".to_string(),
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(result, TaskState::Done);
        }

        #[test]
        fn from_published_verified_illegal_event_preflight_started() {
            let state = TaskState::PublishedVerified {
                commit: "abc123".to_string(),
            };
            let event = EventKind::PreflightStarted;
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("PublishedVerified"));
        }

        #[test]
        fn from_paused_resumed() {
            let resume_to = Box::new(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            });
            let state = TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: resume_to.clone(),
            };
            let event = EventKind::Resumed;
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement
                }
            );
        }

        #[test]
        fn from_paused_new_pause_reason() {
            let resume_to = Box::new(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            });
            let state = TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: resume_to.clone(),
            };
            let event = EventKind::Paused {
                reason: PauseReason::HumanGate,
            };
            let err = apply(&state, &event).expect_err("nested pause should be rejected");
            assert!(err.to_string().contains("Paused"));
        }

        #[test]
        fn from_paused_illegal_event_task_queued() {
            let state = TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Queued),
            };
            let event = EventKind::TaskQueued {
                title: "Task".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Paused"));
        }

        #[test]
        fn terminal_state_done_rejects_all_events() {
            let state = TaskState::Done;
            let event = EventKind::TaskQueued {
                title: "Task".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Done"));
        }

        #[test]
        fn terminal_state_failed_rejects_all_events() {
            let state = TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "Failed".to_string(),
            };
            let event = EventKind::TaskQueued {
                title: "Task".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Failed"));
        }

        #[test]
        fn terminal_state_cancelled_rejects_all_events() {
            let state = TaskState::Cancelled;
            let event = EventKind::TaskQueued {
                title: "Task".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Cancelled"));
        }

        #[test]
        fn terminal_state_acknowledged_rejects_all_events() {
            let state = TaskState::Acknowledged {
                by: "user@example.com".to_string(),
                at: OffsetDateTime::now_utc(),
            };
            let event = EventKind::TaskQueued {
                title: "Task".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Acknowledged"));
        }

        #[test]
        fn running_with_mismatched_attempt_is_rejected() {
            let state = TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            };
            let event = EventKind::VerifyPassed {
                attempt: AttemptId::new(2),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Running"));
        }

        #[test]
        fn remediating_with_lower_attempt_is_rejected() {
            let state = TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            };
            let event = EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: 1234,
                base_sha: "abc".to_string(),
            };
            let err = apply(&state, &event).expect_err("should be invalid");
            assert!(err.to_string().contains("Remediating"));
        }

        #[test]
        fn paused_state_with_interrupted_event() {
            let state = TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            };
            let event = EventKind::Interrupted {
                phase: Phase::Implement,
            };
            let result = apply(&state, &event).expect("transition");
            assert_eq!(
                result,
                TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    resume_to: Box::new(TaskState::Running {
                        attempt: AttemptId::new(1),
                        phase: Phase::Implement
                    })
                }
            );
        }

        #[test]
        #[allow(clippy::too_many_lines)]
        fn every_illegal_transition_is_rejected() {
            use std::collections::HashSet;

            type EventConstructor = Box<dyn Fn(AttemptId) -> EventKind>;

            let attempt1 = AttemptId::new(1);

            let representative_states = vec![
                ("Queued", TaskState::Queued),
                ("Preflight", TaskState::Preflight),
                (
                    "Running",
                    TaskState::Running {
                        attempt: attempt1,
                        phase: Phase::Implement,
                    },
                ),
                (
                    "Remediating",
                    TaskState::Remediating {
                        attempt: attempt1,
                        phase: Phase::Red,
                    },
                ),
                ("Verifying", TaskState::Verifying { attempt: attempt1 }),
                ("Publishing", TaskState::Publishing { attempt: attempt1 }),
                (
                    "PublishedVerified",
                    TaskState::PublishedVerified {
                        commit: "abc123".to_string(),
                    },
                ),
                (
                    "Paused(Running)",
                    TaskState::Paused {
                        reason: PauseReason::Input,
                        resume_to: Box::new(TaskState::Running {
                            attempt: attempt1,
                            phase: Phase::Implement,
                        }),
                    },
                ),
                ("Done", TaskState::Done),
                (
                    "Failed",
                    TaskState::Failed {
                        class: FailureClass::AgentFailure,
                        detail: "failed".to_string(),
                    },
                ),
                (
                    "Acknowledged",
                    TaskState::Acknowledged {
                        by: "user".to_string(),
                        at: OffsetDateTime::now_utc(),
                    },
                ),
                ("Cancelled", TaskState::Cancelled),
            ];

            let event_constructors: Vec<(&str, EventConstructor)> = vec![
                (
                    "TaskQueued",
                    Box::new(|_| EventKind::TaskQueued {
                        title: "Test".to_string(),
                    }),
                ),
                (
                    "PreflightStarted",
                    Box::new(|_| EventKind::PreflightStarted),
                ),
                (
                    "PreflightPassed",
                    Box::new(|_| EventKind::PreflightPassed {
                        base_sha: "abc123".to_string(),
                    }),
                ),
                (
                    "PreflightFailed",
                    Box::new(|_| EventKind::PreflightFailed {
                        class: FailureClass::EnvironmentFailure,
                        detail: "Missing tool".to_string(),
                    }),
                ),
                (
                    "AttemptStarted",
                    Box::new(|attempt| EventKind::AttemptStarted {
                        attempt,
                        protocol: "direct".to_string(),
                        pid: 1234,
                        base_sha: "abc123".to_string(),
                    }),
                ),
                (
                    "PhaseEntered",
                    Box::new(|attempt| EventKind::PhaseEntered {
                        attempt,
                        phase: Phase::Implement,
                    }),
                ),
                (
                    "AgentOutput",
                    Box::new(|attempt| EventKind::AgentOutput {
                        attempt,
                        stream: crate::classify::Stream::Stdout,
                        text: "output".to_string(),
                    }),
                ),
                (
                    "VerifyPassed",
                    Box::new(|attempt| EventKind::VerifyPassed { attempt }),
                ),
                (
                    "VerifyFailed",
                    Box::new(|attempt| EventKind::VerifyFailed {
                        attempt,
                        class: FailureClass::VerificationFailure,
                        detail: "Test failed".to_string(),
                    }),
                ),
                (
                    "PublishStarted",
                    Box::new(|attempt| EventKind::PublishStarted {
                        attempt,
                        candidate_sha: "abc123".to_string(),
                    }),
                ),
                (
                    "PublishVerified",
                    Box::new(|_| EventKind::PublishVerified {
                        commit: "abc123".to_string(),
                        remote_sha: "abc123".to_string(),
                    }),
                ),
                (
                    "TaskDone",
                    Box::new(|_| EventKind::TaskDone {
                        commit: "abc123".to_string(),
                    }),
                ),
                (
                    "TaskFailed",
                    Box::new(|_| EventKind::TaskFailed {
                        class: FailureClass::AgentFailure,
                        detail: "failed".to_string(),
                    }),
                ),
                (
                    "TaskCancelled",
                    Box::new(|_| EventKind::TaskCancelled {
                        reason: "user requested".to_string(),
                    }),
                ),
                (
                    "Paused",
                    Box::new(|_| EventKind::Paused {
                        reason: PauseReason::Input,
                    }),
                ),
                ("Resumed", Box::new(|_| EventKind::Resumed)),
                (
                    "Interrupted",
                    Box::new(|_| EventKind::Interrupted {
                        phase: Phase::Implement,
                    }),
                ),
                (
                    "RecoveryDecision",
                    Box::new(|_| EventKind::RecoveryDecision {
                        decision: crate::classify::Recovery::Resume,
                        detail: "resuming".to_string(),
                    }),
                ),
                (
                    "GateAcknowledged",
                    Box::new(|_| EventKind::GateAcknowledged {
                        by: "user".to_string(),
                        at: OffsetDateTime::now_utc(),
                    }),
                ),
            ];

            let legal_transitions: HashSet<(&str, &str)> = vec![
                ("Queued", "TaskQueued"),
                ("Queued", "PreflightStarted"),
                ("Queued", "Paused"),
                ("Queued", "TaskCancelled"),
                ("Preflight", "PreflightStarted"),
                ("Preflight", "PreflightPassed"),
                ("Preflight", "PreflightFailed"),
                ("Preflight", "AttemptStarted"),
                ("Preflight", "Paused"),
                ("Preflight", "TaskCancelled"),
                ("Running", "PhaseEntered"),
                ("Running", "AgentOutput"),
                ("Running", "VerifyPassed"),
                ("Running", "VerifyFailed"),
                ("Running", "TaskFailed"),
                ("Running", "Paused"),
                ("Running", "Interrupted"),
                ("Running", "TaskCancelled"),
                ("Remediating", "PhaseEntered"),
                ("Remediating", "AgentOutput"),
                ("Remediating", "AttemptStarted"),
                ("Remediating", "VerifyPassed"),
                ("Remediating", "VerifyFailed"),
                ("Remediating", "TaskFailed"),
                ("Remediating", "Paused"),
                ("Remediating", "Interrupted"),
                ("Remediating", "TaskCancelled"),
                ("Verifying", "PublishStarted"),
                ("Verifying", "VerifyFailed"),
                ("Verifying", "TaskFailed"),
                ("Verifying", "Paused"),
                ("Verifying", "Interrupted"),
                ("Verifying", "TaskCancelled"),
                ("Publishing", "PublishStarted"),
                ("Publishing", "PublishVerified"),
                ("Publishing", "TaskFailed"),
                ("Publishing", "Paused"),
                ("Publishing", "Interrupted"),
                ("Publishing", "TaskCancelled"),
                ("PublishedVerified", "TaskDone"),
                ("PublishedVerified", "Paused"),
                ("PublishedVerified", "Interrupted"),
                ("PublishedVerified", "TaskCancelled"),
                ("Paused(Running)", "Resumed"),
                ("Paused(Running)", "TaskCancelled"),
            ]
            .into_iter()
            .collect();

            for (state_name, state) in &representative_states {
                for (event_name, event_fn) in &event_constructors {
                    let attempt_for_event =
                        if *state_name == "Remediating" && *event_name == "AttemptStarted" {
                            AttemptId::new(2)
                        } else {
                            attempt1
                        };

                    let event = event_fn(attempt_for_event);
                    let result = apply(state, &event);
                    let is_legal = legal_transitions.contains(&(state_name, event_name));

                    assert_eq!(
                        result.is_ok(),
                        is_legal,
                        "Transition {state_name}+{event_name}: expected legal={is_legal}, got result={result:?}"
                    );
                }
            }
        }

        #[test]
        fn pause_and_resume_returns_original_state_for_all_non_terminal_states() {
            let attempt1 = AttemptId::new(1);

            let non_terminal_states = vec![
                ("Queued", TaskState::Queued),
                ("Preflight", TaskState::Preflight),
                (
                    "Running",
                    TaskState::Running {
                        attempt: attempt1,
                        phase: Phase::Implement,
                    },
                ),
                (
                    "Remediating",
                    TaskState::Remediating {
                        attempt: attempt1,
                        phase: Phase::Red,
                    },
                ),
                ("Verifying", TaskState::Verifying { attempt: attempt1 }),
                ("Publishing", TaskState::Publishing { attempt: attempt1 }),
                (
                    "PublishedVerified",
                    TaskState::PublishedVerified {
                        commit: "abc123".to_string(),
                    },
                ),
            ];

            for (state_name, state) in non_terminal_states {
                let pause_event = EventKind::Paused {
                    reason: PauseReason::Input,
                };
                let paused_state =
                    apply(&state, &pause_event).unwrap_or_else(|_| panic!("pause {state_name}"));
                assert!(
                    paused_state.is_paused(),
                    "{state_name} should be paused after Paused event"
                );

                let resume_event = EventKind::Resumed;
                let resumed_state = apply(&paused_state, &resume_event)
                    .unwrap_or_else(|_| panic!("resume from {state_name}"));
                assert_eq!(
                    resumed_state, state,
                    "{state_name}: resumed state should equal original state"
                );
            }
        }

        #[test]
        fn nested_pauses_are_rejected_from_all_pause_reasons() {
            let pause_reasons = vec![
                PauseReason::Limit { until: None },
                PauseReason::Input,
                PauseReason::HumanGate,
                PauseReason::Interrupted,
                PauseReason::Blocked,
            ];

            for initial_reason in &pause_reasons {
                let state = TaskState::Paused {
                    reason: initial_reason.clone(),
                    resume_to: Box::new(TaskState::Queued),
                };

                for new_reason in &pause_reasons {
                    let event = EventKind::Paused {
                        reason: new_reason.clone(),
                    };
                    let err = apply(&state, &event).expect_err("nested pause should be rejected");
                    assert!(
                        err.to_string().contains("Paused"),
                        "Error should mention Paused state"
                    );
                }
            }
        }

        #[test]
        fn pausing_terminal_states_is_rejected() {
            let terminal_states = vec![
                ("Done", TaskState::Done),
                (
                    "Failed",
                    TaskState::Failed {
                        class: FailureClass::AgentFailure,
                        detail: "failed".to_string(),
                    },
                ),
                ("Cancelled", TaskState::Cancelled),
                (
                    "Acknowledged",
                    TaskState::Acknowledged {
                        by: "user".to_string(),
                        at: OffsetDateTime::now_utc(),
                    },
                ),
            ];

            for (state_name, state) in terminal_states {
                let pause_event = EventKind::Paused {
                    reason: PauseReason::Input,
                };
                let err = apply(&state, &pause_event)
                    .expect_err(&format!("pausing terminal state {state_name}"));
                assert!(
                    err.to_string().contains(state_name),
                    "Error should mention {state_name}"
                );
            }
        }
    }
}

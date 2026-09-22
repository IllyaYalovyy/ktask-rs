//! `TaskState`, `Phase` and `PauseReason`: the supervisor's lifecycle
//! vocabulary — what state a task is in, where an attempt stands within a
//! protocol, and why a task is currently paused.
//!
//! All three are plain data, defined exactly as `docs/DESIGN.md` states them
//! under "Core types" and "Phases and screens": no logic, no I/O. The
//! `apply` function that transitions `TaskState` belongs to a later task.

use crate::classify::FailureClass;
use crate::ids::AttemptId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Where a task stands in the supervisor's custody of it.
///
/// Pipeline states move left to right: `Queued -> Preflight -> Running ->
/// Verifying -> Publishing -> PublishedVerified -> Done`, with `Running`
/// able to loop back through `Remediating` (bounded by
/// `max_remediation_attempts`). `Paused` suspends any of those pipeline
/// states and remembers exactly where to resume. `Done`, `Acknowledged`,
/// `Failed` and `Cancelled` are terminal, per `VISION.md` §6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskState {
    /// Waiting for its turn; no attempt has started.
    Queued,
    /// Proving the world is sane before spending tokens: clean fetched
    /// mainline, green baseline, provider available, disk space, lock held.
    Preflight,
    /// An attempt is executing the named phase of its protocol.
    Running {
        /// Which attempt is running.
        attempt: AttemptId,
        /// The phase it is currently in.
        phase: Phase,
    },
    /// A bounded retry after `Running` failed verification, executing the
    /// named phase of its protocol.
    Remediating {
        /// Which attempt is remediating.
        attempt: AttemptId,
        /// The phase it is currently in.
        phase: Phase,
    },
    /// Running the mandatory completion gates against the attempt's result.
    Verifying {
        /// Which attempt is being verified.
        attempt: AttemptId,
    },
    /// Publishing the verified result to mainline.
    Publishing {
        /// Which attempt is being published.
        attempt: AttemptId,
    },
    /// Published and confirmed present on the remote at `commit`.
    PublishedVerified {
        /// The commit SHA confirmed on mainline.
        commit: String,
    },
    /// Fully complete: a terminal success state for an executable task.
    Done,
    /// A gate's terminal success state: a human confirmed it via `ack`.
    Acknowledged {
        /// Who acknowledged the gate.
        by: String,
        /// When they acknowledged it.
        at: OffsetDateTime,
    },
    /// Suspended, remembering the state to resume into.
    Paused {
        /// Why the task is paused.
        reason: PauseReason,
        /// The state to return to once the pause is resolved.
        resume_to: Box<TaskState>,
    },
    /// Terminally failed; no further attempts will run.
    Failed {
        /// The class of failure.
        class: FailureClass,
        /// A human-readable description of what went wrong.
        detail: String,
    },
    /// Terminally cancelled by a human.
    Cancelled,
}

impl TaskState {
    /// Whether this state is terminal: no further transition occurs without
    /// external intervention (a new task, a requeue, or similar).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Done
                | TaskState::Acknowledged { .. }
                | TaskState::Failed { .. }
                | TaskState::Cancelled
        )
    }

    /// Whether this state is a durable pause: the task is suspended and
    /// remembers where to resume.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self, TaskState::Paused { .. })
    }

    /// The variant's name, stable for logging, display and error messages.
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

/// A step within a protocol's execution of an attempt.
///
/// Carries every phase any protocol needs — including `spec-first`'s
/// `Goal`, `Scope`, `AcceptanceTests`, `Review`, `Harden` and `DoneCheck` —
/// so no later task has to widen this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// State the outcome the task must achieve.
    Goal,
    /// State what is, and is not, in scope.
    Scope,
    /// Write the tests that prove the outcome was achieved.
    AcceptanceTests,
    /// Implement, for protocols that do not separate red/green/refactor.
    Implement,
    /// Write a failing test; production code paths are read-only.
    Red,
    /// Make the failing test pass with the smallest change that does so.
    Green,
    /// Clean up while the tests from `Red`/`Green` stay green.
    Refactor,
    /// Review the change before hardening it.
    Review,
    /// Address edge cases and robustness.
    Harden,
    /// Check the task's `Done-when` criteria are satisfied.
    DoneCheck,
    /// Run the mandatory completion gates.
    Verify,
    /// Publish the verified result.
    Publish,
}

/// Why a task is currently paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// A provider usage limit was hit; resumes after `until`, when known.
    Limit {
        /// When the limit is expected to lift, if the provider reported one.
        until: Option<OffsetDateTime>,
    },
    /// The task is blocked on information only a human can supply.
    Input,
    /// The task is blocked on a human's explicit approval to proceed.
    HumanGate,
    /// The run was interrupted, for example by a process kill or a restart.
    Interrupted,
    /// The task is blocked on another task or an external condition.
    Blocked,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_phases() -> Vec<Phase> {
        vec![
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
        ]
    }

    #[test]
    fn phase_has_exactly_twelve_variants() {
        let variants = all_phases();
        assert_eq!(variants.len(), 12);

        // Exhaustive, wildcard-free match: if a variant is ever added to
        // `Phase` without being listed here too, this stops compiling
        // instead of silently under-counting.
        for phase in variants {
            match phase {
                Phase::Goal
                | Phase::Scope
                | Phase::AcceptanceTests
                | Phase::Implement
                | Phase::Red
                | Phase::Green
                | Phase::Refactor
                | Phase::Review
                | Phase::Harden
                | Phase::DoneCheck
                | Phase::Verify
                | Phase::Publish => {}
            }
        }
    }

    #[test]
    fn every_phase_round_trips_through_json() {
        for phase in all_phases() {
            let json = serde_json::to_string(&phase).expect("serialize");
            let back: Phase = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(phase, back);
        }
    }

    fn all_pause_reasons() -> Vec<PauseReason> {
        vec![
            PauseReason::Limit { until: None },
            PauseReason::Input,
            PauseReason::HumanGate,
            PauseReason::Interrupted,
            PauseReason::Blocked,
        ]
    }

    #[test]
    fn pause_reason_has_exactly_five_variants() {
        let variants = all_pause_reasons();
        assert_eq!(variants.len(), 5);

        for reason in variants {
            match reason {
                PauseReason::Limit { until: _ }
                | PauseReason::Input
                | PauseReason::HumanGate
                | PauseReason::Interrupted
                | PauseReason::Blocked => {}
            }
        }
    }

    #[test]
    fn every_pause_reason_round_trips_through_json() {
        for reason in all_pause_reasons() {
            let json = serde_json::to_string(&reason).expect("serialize");
            let back: PauseReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(reason, back);
        }
    }

    #[test]
    fn pause_reason_limit_with_a_known_time_round_trips_through_json() {
        let reason = PauseReason::Limit {
            until: Some(OffsetDateTime::UNIX_EPOCH),
        };
        let json = serde_json::to_string(&reason).expect("serialize");
        let back: PauseReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(reason, back);
    }

    fn all_task_states() -> Vec<TaskState> {
        vec![
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Harden,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            TaskState::PublishedVerified {
                commit: "abc123".to_string(),
            },
            TaskState::Done,
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Green,
                }),
            },
            TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: "tests failed".to_string(),
            },
            TaskState::Cancelled,
        ]
    }

    #[test]
    fn task_state_has_exactly_twelve_variants() {
        let variants = all_task_states();
        assert_eq!(variants.len(), 12);

        // Exhaustive, wildcard-free match: if a variant is ever added to
        // `TaskState` without being listed here too, this stops compiling
        // instead of silently under-counting.
        for state in variants {
            match state {
                TaskState::Queued
                | TaskState::Preflight
                | TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
                | TaskState::Publishing { .. }
                | TaskState::PublishedVerified { .. }
                | TaskState::Done
                | TaskState::Acknowledged { .. }
                | TaskState::Paused { .. }
                | TaskState::Failed { .. }
                | TaskState::Cancelled => {}
            }
        }
    }

    #[test]
    fn every_task_state_round_trips_through_json() {
        for state in all_task_states() {
            let json = serde_json::to_string(&state).expect("serialize");
            let back: TaskState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(state, back);
        }
    }

    #[test]
    fn paused_state_round_trips_its_boxed_resume_state_through_json() {
        let state = TaskState::Paused {
            reason: PauseReason::Limit {
                until: Some(OffsetDateTime::UNIX_EPOCH),
            },
            resume_to: Box::new(TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Verifying {
                    attempt: AttemptId::new(3),
                }),
            }),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: TaskState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, back);

        match back {
            TaskState::Paused { resume_to, .. } => match *resume_to {
                TaskState::Paused { resume_to, .. } => {
                    assert_eq!(
                        *resume_to,
                        TaskState::Verifying {
                            attempt: AttemptId::new(3)
                        }
                    );
                }
                other => panic!("expected nested Paused, got {other:?}"),
            },
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[test]
    fn terminal_states_report_is_terminal_true() {
        assert!(TaskState::Done.is_terminal());
        assert!(
            TaskState::Acknowledged {
                by: "bob".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            }
            .is_terminal()
        );
        assert!(
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "crashed".to_string(),
            }
            .is_terminal()
        );
        assert!(TaskState::Cancelled.is_terminal());
    }

    #[test]
    fn non_terminal_states_report_is_terminal_false() {
        for state in all_task_states() {
            let expect_terminal = matches!(
                state,
                TaskState::Done
                    | TaskState::Acknowledged { .. }
                    | TaskState::Failed { .. }
                    | TaskState::Cancelled
            );
            assert_eq!(state.is_terminal(), expect_terminal, "state: {state:?}");
        }
    }

    #[test]
    fn only_paused_reports_is_paused_true() {
        for state in all_task_states() {
            let expect_paused = matches!(state, TaskState::Paused { .. });
            assert_eq!(state.is_paused(), expect_paused, "state: {state:?}");
        }
    }

    #[test]
    fn name_returns_the_variant_name_for_every_state() {
        assert_eq!(TaskState::Queued.name(), "Queued");
        assert_eq!(TaskState::Preflight.name(), "Preflight");
        assert_eq!(
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }
            .name(),
            "Running"
        );
        assert_eq!(
            TaskState::Remediating {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
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
                commit: "sha".to_string()
            }
            .name(),
            "PublishedVerified"
        );
        assert_eq!(TaskState::Done.name(), "Done");
        assert_eq!(
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            }
            .name(),
            "Acknowledged"
        );
        assert_eq!(
            TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Queued),
            }
            .name(),
            "Paused"
        );
        assert_eq!(
            TaskState::Failed {
                class: FailureClass::PolicyFailure,
                detail: "nope".to_string(),
            }
            .name(),
            "Failed"
        );
        assert_eq!(TaskState::Cancelled.name(), "Cancelled");
    }
}

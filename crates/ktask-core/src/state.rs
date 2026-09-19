//! Task state transitions and phases.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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

//! `Phase` and `PauseReason`: vocabulary for where an attempt stands within
//! a protocol, and why a task is currently paused.
//!
//! Both are plain data, defined exactly as `docs/DESIGN.md` states them
//! under "Core types" and "Phases and screens": no logic, no I/O. `TaskState`
//! and the `apply` function that transitions it belong to a later task.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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
}

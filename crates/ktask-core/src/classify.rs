//! Vocabulary for classifying why an attempt failed, which stream a line of
//! agent output came from, how the runner recovers from an interruption,
//! and which `tdd` protocol exception a task invoked.
//!
//! `FailureClass` is defined exactly as `docs/DESIGN.md` states it under
//! "Core types". `Stream` and `Recovery` come from the same document's event
//! catalog. `TddException` encodes the four exception categories named in
//! `VISION.md` §9 ("documentation, pure refactoring, build configuration,
//! and bugs already covered by a failing test"). All four are plain data:
//! no logic, no I/O. `classify()`, which produces a `FailureClass` from a
//! real failure, belongs to a later task.

use serde::{Deserialize, Serialize};

/// Why an attempt or a gate failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// The agent itself failed: a non-zero exit, a crash, a refusal.
    AgentFailure,
    /// A verification gate (tests, lint, build, ...) failed.
    VerificationFailure,
    /// The provider reported a usage limit.
    ProviderLimit,
    /// The provider failed transiently and a retry may succeed.
    ProviderTransient,
    /// The provider is misconfigured (bad credentials, wrong model, ...).
    ProviderConfiguration,
    /// Publishing conflicted with mainline.
    GitConflict,
    /// The environment failed independent of the agent or the provider,
    /// for example a full disk.
    EnvironmentFailure,
    /// A policy (write scope, secret redaction, ...) was violated.
    PolicyFailure,
    /// The task is blocked on information only a human can supply.
    NeedsInput,
}

/// Which stream a line of agent output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// How the runner reconciles a task's recorded state with reality after an
/// interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovery {
    /// The interrupted work can resume where it left off.
    Resume,
    /// The interrupted work cannot be trusted and is marked interrupted.
    MarkInterrupted,
    /// The work the journal describes was already applied; nothing to redo.
    AlreadyApplied,
}

/// A recognized exception to the `tdd` protocol's red/green/refactor
/// ordering, recorded in task history whenever it is invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TddException {
    /// The change is documentation only.
    Documentation,
    /// The change is a pure refactor: behavior is unchanged and existing
    /// tests already cover it.
    PureRefactoring,
    /// The change is build configuration, not production behavior.
    BuildConfiguration,
    /// The change fixes a bug a failing test already covered before this
    /// attempt began.
    PreExistingFailingTest,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_failure_classes() -> Vec<FailureClass> {
        vec![
            FailureClass::AgentFailure,
            FailureClass::VerificationFailure,
            FailureClass::ProviderLimit,
            FailureClass::ProviderTransient,
            FailureClass::ProviderConfiguration,
            FailureClass::GitConflict,
            FailureClass::EnvironmentFailure,
            FailureClass::PolicyFailure,
            FailureClass::NeedsInput,
        ]
    }

    #[test]
    fn failure_class_has_exactly_nine_variants() {
        let variants = all_failure_classes();
        assert_eq!(variants.len(), 9);

        // Exhaustive, wildcard-free match: a variant added to `FailureClass`
        // without being listed here fails to compile instead of silently
        // under-counting.
        for class in variants {
            match class {
                FailureClass::AgentFailure
                | FailureClass::VerificationFailure
                | FailureClass::ProviderLimit
                | FailureClass::ProviderTransient
                | FailureClass::ProviderConfiguration
                | FailureClass::GitConflict
                | FailureClass::EnvironmentFailure
                | FailureClass::PolicyFailure
                | FailureClass::NeedsInput => {}
            }
        }
    }

    #[test]
    fn every_failure_class_round_trips_through_json() {
        for class in all_failure_classes() {
            let json = serde_json::to_string(&class).expect("serialize");
            let back: FailureClass = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(class, back);
        }
    }

    fn all_streams() -> Vec<Stream> {
        vec![Stream::Stdout, Stream::Stderr]
    }

    #[test]
    fn stream_has_exactly_two_variants() {
        let variants = all_streams();
        assert_eq!(variants.len(), 2);

        for stream in variants {
            match stream {
                Stream::Stdout | Stream::Stderr => {}
            }
        }
    }

    #[test]
    fn every_stream_round_trips_through_json() {
        for stream in all_streams() {
            let json = serde_json::to_string(&stream).expect("serialize");
            let back: Stream = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(stream, back);
        }
    }

    fn all_recoveries() -> Vec<Recovery> {
        vec![
            Recovery::Resume,
            Recovery::MarkInterrupted,
            Recovery::AlreadyApplied,
        ]
    }

    #[test]
    fn recovery_has_exactly_three_variants() {
        let variants = all_recoveries();
        assert_eq!(variants.len(), 3);

        for recovery in variants {
            match recovery {
                Recovery::Resume | Recovery::MarkInterrupted | Recovery::AlreadyApplied => {}
            }
        }
    }

    #[test]
    fn every_recovery_round_trips_through_json() {
        for recovery in all_recoveries() {
            let json = serde_json::to_string(&recovery).expect("serialize");
            let back: Recovery = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(recovery, back);
        }
    }

    fn all_tdd_exceptions() -> Vec<TddException> {
        vec![
            TddException::Documentation,
            TddException::PureRefactoring,
            TddException::BuildConfiguration,
            TddException::PreExistingFailingTest,
        ]
    }

    #[test]
    fn tdd_exception_has_exactly_four_variants() {
        let variants = all_tdd_exceptions();
        assert_eq!(variants.len(), 4);

        for exception in variants {
            match exception {
                TddException::Documentation
                | TddException::PureRefactoring
                | TddException::BuildConfiguration
                | TddException::PreExistingFailingTest => {}
            }
        }
    }

    #[test]
    fn every_tdd_exception_round_trips_through_json() {
        for exception in all_tdd_exceptions() {
            let json = serde_json::to_string(&exception).expect("serialize");
            let back: TddException = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(exception, back);
        }
    }
}

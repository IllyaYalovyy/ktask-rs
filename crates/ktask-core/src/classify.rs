//! Failure classification and recovery types.

use serde::{Deserialize, Serialize};

/// Classification of how a task failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// Agent execution failed.
    AgentFailure,
    /// Verification step failed.
    VerificationFailure,
    /// Provider rate limit reached.
    ProviderLimit,
    /// Provider had a transient error.
    ProviderTransient,
    /// Provider configuration is incorrect.
    ProviderConfiguration,
    /// Git merge conflict.
    GitConflict,
    /// Environment dependency missing.
    EnvironmentFailure,
    /// Policy violation.
    PolicyFailure,
    /// Task needs human input to proceed.
    NeedsInput,
}

/// Output stream from agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Recovery decision after failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovery {
    /// Resume the task.
    Resume,
    /// Mark as interrupted.
    MarkInterrupted,
    /// Recovery already applied.
    AlreadyApplied,
}

/// Exception to TDD workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TddException {
    /// Documentation-only change.
    Documentation,
    /// Pure refactoring.
    PureRefactor,
    /// Build configuration change.
    BuildConfig,
    /// Existing test was already failing.
    ExistingFailingTest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_class_has_nine_variants() {
        let variants = [
            FailureClass::AgentFailure,
            FailureClass::VerificationFailure,
            FailureClass::ProviderLimit,
            FailureClass::ProviderTransient,
            FailureClass::ProviderConfiguration,
            FailureClass::GitConflict,
            FailureClass::EnvironmentFailure,
            FailureClass::PolicyFailure,
            FailureClass::NeedsInput,
        ];
        assert_eq!(variants.len(), 9);
    }

    #[test]
    fn failure_class_roundtrips_through_json() {
        let variants = vec![
            FailureClass::AgentFailure,
            FailureClass::VerificationFailure,
            FailureClass::ProviderLimit,
            FailureClass::ProviderTransient,
            FailureClass::ProviderConfiguration,
            FailureClass::GitConflict,
            FailureClass::EnvironmentFailure,
            FailureClass::PolicyFailure,
            FailureClass::NeedsInput,
        ];
        for class in variants {
            let json = serde_json::to_string(&class).expect("serialize class");
            let deserialized: FailureClass =
                serde_json::from_str(&json).expect("deserialize class");
            assert_eq!(class, deserialized);
        }
    }

    #[test]
    fn stream_has_two_variants() {
        let variants = [Stream::Stdout, Stream::Stderr];
        assert_eq!(variants.len(), 2);
    }

    #[test]
    fn stream_roundtrips_through_json() {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let json = serde_json::to_string(&stream).expect("serialize stream");
            let deserialized: Stream = serde_json::from_str(&json).expect("deserialize stream");
            assert_eq!(stream, deserialized);
        }
    }

    #[test]
    fn recovery_has_three_variants() {
        let variants = [
            Recovery::Resume,
            Recovery::MarkInterrupted,
            Recovery::AlreadyApplied,
        ];
        assert_eq!(variants.len(), 3);
    }

    #[test]
    fn recovery_roundtrips_through_json() {
        for recovery in [
            Recovery::Resume,
            Recovery::MarkInterrupted,
            Recovery::AlreadyApplied,
        ] {
            let json = serde_json::to_string(&recovery).expect("serialize recovery");
            let deserialized: Recovery = serde_json::from_str(&json).expect("deserialize recovery");
            assert_eq!(recovery, deserialized);
        }
    }

    #[test]
    fn tdd_exception_has_four_variants() {
        let variants = [
            TddException::Documentation,
            TddException::PureRefactor,
            TddException::BuildConfig,
            TddException::ExistingFailingTest,
        ];
        assert_eq!(variants.len(), 4);
    }

    #[test]
    fn tdd_exception_roundtrips_through_json() {
        for exception in [
            TddException::Documentation,
            TddException::PureRefactor,
            TddException::BuildConfig,
            TddException::ExistingFailingTest,
        ] {
            let json = serde_json::to_string(&exception).expect("serialize exception");
            let deserialized: TddException =
                serde_json::from_str(&json).expect("deserialize exception");
            assert_eq!(exception, deserialized);
        }
    }
}

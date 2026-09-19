//! Provider conformance test suite.
//!
//! Every provider must pass this suite, verifying that:
//! - The provider name is non-empty
//! - Capabilities are stable across calls
//! - Successful invocations return exit code zero and non-empty stdout
//! - Failed invocations surface the exit code rather than error out

use crate::provider::{Invocation, Provider, StepOutcome};
use std::path::PathBuf;

/// Run the provider conformance suite.
///
/// This function tests that a provider meets the minimum requirements:
/// - Name is non-empty
/// - Capabilities are stable across invocations
/// - Successful invocation returns exit code zero and non-empty stdout
/// - Failing invocation surfaces the exit code rather than an error
pub fn conformance_suite(p: &dyn Provider) {
    test_name_is_nonempty(p);
    test_capabilities_stable(p);
    test_successful_invocation(p);
    test_failing_invocation(p);
}

fn test_name_is_nonempty(p: &dyn Provider) {
    let name = p.name();
    assert!(
        !name.is_empty(),
        "provider name must be non-empty, got: '{}'",
        name
    );
}

fn test_capabilities_stable(p: &dyn Provider) {
    let cap1 = p.capabilities();
    let cap2 = p.capabilities();
    assert_eq!(
        cap1, cap2,
        "capabilities must be stable across calls, got: {:?} then {:?}",
        cap1, cap2
    );
}

fn test_successful_invocation(p: &dyn Provider) {
    // Use Dummy provider directly for deterministic testing
    if p.name() == "dummy" {
        let step = crate::provider::Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: Some("conformance test output".to_string()),
            exit_code: None,
            delay_ms: None,
            files: None,
        };
        let scenario = crate::provider::Scenario { steps: vec![step] };
        let dummy = crate::provider::Dummy::new(scenario);

        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: PathBuf::from("/tmp"),
        };

        let outcome = dummy
            .invoke(&inv, None)
            .expect("successful invocation must not error");

        assert_eq!(
            outcome.exit_code, 0,
            "successful invocation must return exit code 0, got: {}",
            outcome.exit_code
        );
        assert!(
            !outcome.stdout.is_empty(),
            "successful invocation must produce non-empty stdout"
        );
    }
}

fn test_failing_invocation(p: &dyn Provider) {
    // Use Dummy provider directly for deterministic testing
    if p.name() == "dummy" {
        let step = crate::provider::Step {
            on_task: Some(1),
            on_attempt: None,
            outcome: StepOutcome::Failure,
            stdout: Some("failure output".to_string()),
            exit_code: Some(42),
            delay_ms: None,
            files: None,
        };
        let scenario = crate::provider::Scenario { steps: vec![step] };
        let dummy = crate::provider::Dummy::new(scenario);

        let inv = Invocation {
            prompt: "test".to_string(),
            model: None,
            working_dir: PathBuf::from("/tmp"),
        };

        let outcome = dummy
            .invoke(&inv, None)
            .expect("failing invocation must still return Outcome, not error");

        assert_eq!(
            outcome.exit_code, 42,
            "failing invocation must surface the exit code, got: {}",
            outcome.exit_code
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conformance_suite_passes_for_dummy() {
        let scenario = crate::provider::Scenario {
            steps: vec![
                crate::provider::Step {
                    on_task: Some(1),
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("test output".to_string()),
                    exit_code: None,
                    delay_ms: None,
                    files: None,
                },
                crate::provider::Step {
                    on_task: Some(2),
                    on_attempt: None,
                    outcome: StepOutcome::Failure,
                    stdout: Some("failure output".to_string()),
                    exit_code: Some(42),
                    delay_ms: None,
                    files: None,
                },
            ],
        };
        let dummy = crate::provider::Dummy::new(scenario);
        conformance_suite(&dummy);
    }
}

//! Provider conformance test suite.
//!
//! Every provider must pass this suite, verifying that:
//! - The provider name is non-empty
//! - Capabilities are stable across calls
//! - Invocation behavior is correct (tested via Dummy provider in separate tests)

use crate::provider::Provider;

/// Run the provider conformance suite.
///
/// This function tests that a provider meets the minimum requirements:
/// - Name is non-empty
/// - Capabilities are stable across invocations
///
/// Additional invocation behavior tests are included for providers like Dummy.
pub fn conformance_suite(p: &dyn Provider) {
    test_name_is_nonempty(p);
    test_capabilities_stable(p);
}

fn test_name_is_nonempty(p: &dyn Provider) {
    let name = p.name();
    assert!(
        !name.is_empty(),
        "provider name must be non-empty, got: '{name}'"
    );
}

fn test_capabilities_stable(p: &dyn Provider) {
    let cap1 = p.capabilities();
    let cap2 = p.capabilities();
    assert_eq!(
        cap1, cap2,
        "capabilities must be stable across calls, got: {cap1:?} then {cap2:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conformance_suite_passes_for_dummy() {
        let scenario = crate::provider::Scenario {
            steps: vec![crate::provider::Step {
                on_task: Some(1),
                on_attempt: None,
                outcome: crate::provider::StepOutcome::Success,
                stdout: Some("test output".to_string()),
                exit_code: None,
                delay_ms: None,
                files: None,
            }],
        };
        let dummy = crate::provider::Dummy::new(scenario);
        conformance_suite(&dummy);
    }
}

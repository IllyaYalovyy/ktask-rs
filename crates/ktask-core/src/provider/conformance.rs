//! One suite every [`Provider`] adapter must pass (`VISION.md` §12, §15).
//!
//! [`conformance_suite`] is the entire contract: an adapter's own tests hand
//! it a provider instance primed to report success on its first
//! [`Provider::invoke`] call and failure on its second, and the suite checks
//! the properties every adapter must share — a non-empty name, capabilities
//! that do not change between calls, a successful invocation reporting exit
//! code zero with non-empty stdout, and a failing invocation surfacing its
//! exit code as a successful `Outcome` rather than an `Err`. Registering a
//! new adapter means calling this function against it, not reimplementing
//! these checks.

use crate::{Invocation, Provider};

/// Runs the provider conformance checks against `p`.
///
/// `p` must already be primed so that its first call to [`Provider::invoke`]
/// reports success (exit code zero, non-empty stdout) and its second call
/// reports failure (a nonzero exit code, still returned as `Ok`) — exactly
/// the shape a two-step `[success, failure]` [`crate::Scenario`] gives
/// [`crate::Dummy`].
///
/// # Panics
///
/// Panics on the first conformance property `p` fails, via `assert!`.
pub(crate) fn conformance_suite(p: &dyn Provider) {
    assert!(!p.name().is_empty(), "provider name must be non-empty");

    let first = p.capabilities();
    let second = p.capabilities();
    assert_eq!(
        first, second,
        "capabilities must be stable across repeated calls"
    );

    let dir = tempfile::tempdir().expect("tempdir for conformance invocation");
    let inv = Invocation {
        prompt: "ktask provider conformance check".to_string(),
        model: None,
        working_dir: dir.path().to_path_buf(),
    };

    let success = p
        .invoke(&inv, None)
        .expect("a successful invocation must return Ok, not Err");
    assert_eq!(
        success.exit_code, 0,
        "a successful invocation must report exit code zero"
    );
    assert!(
        !success.stdout.is_empty(),
        "a successful invocation must report non-empty stdout"
    );

    let failure = p.invoke(&inv, None).expect(
        "a failing invocation must surface as an Ok outcome with a nonzero exit code, not an Err",
    );
    assert_ne!(
        failure.exit_code, 0,
        "a failing invocation's nonzero exit code must reach the caller, not be swallowed as an error"
    );
}

#[cfg(test)]
mod dummy_conformance {
    use super::conformance_suite;
    use crate::{Dummy, Scenario, Step, StepOutcome};

    fn step(outcome: StepOutcome, stdout: &str, exit_code: i32) -> Step {
        Step {
            on_task: None,
            on_attempt: None,
            outcome,
            stdout: Some(stdout.to_string()),
            exit_code: Some(exit_code),
            delay_ms: None,
            files: Vec::new(),
        }
    }

    /// `Done-when`: the suite runs for `Dummy` in the default test run.
    #[test]
    fn dummy_passes_the_provider_conformance_suite() {
        let dummy = Dummy::new(Scenario {
            steps: vec![
                step(StepOutcome::Success, "conformance ok\n", 0),
                step(StepOutcome::Failure, "conformance failure\n", 3),
            ],
        });

        conformance_suite(&dummy);
    }
}

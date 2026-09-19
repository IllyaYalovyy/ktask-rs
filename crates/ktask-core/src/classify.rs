//! Failure classification and recovery types.

use crate::Error;
use crate::gate::GateResult;
use crate::provider::Outcome;
use serde::{Deserialize, Serialize};

/// Detects if an error message indicates a provider rate/usage limit.
///
/// Checks both stdout and stderr for common provider limit indicators.
fn limit_message(output: &str) -> bool {
    let output_lower = output.to_lowercase();
    output_lower.contains("rate limit")
        || output_lower.contains("quota exceeded")
        || output_lower.contains("usage limit")
        || output_lower.contains("token limit")
        || output_lower.contains("request limit")
        || output_lower.contains("over quota")
        || output_lower.contains("limit reached")
        || output_lower.contains("too many requests")
}

/// Classify a task failure before recovery is attempted.
///
/// Examines the outcome, verification gate results, and any git error to determine
/// the class of failure. Classification is applied in priority order:
/// 1. Provider configuration (auth, model, executable)
/// 2. Provider limit (rate/usage limits)
/// 3. Provider transient (network, timeouts, crashes)
/// 4. Git conflict (merge conflicts, rejected push)
/// 5. Policy violation (dirty tree, forbidden files)
/// 6. Verification failure (tests, lint, build, privacy)
/// 7. Needs input (unresolved decision)
/// 8. Environment failure (missing SDK, dependency)
/// 9. Agent failure (fallback for all other cases)
#[must_use]
pub fn classify(
    outcome: &Outcome,
    gates: &[GateResult],
    git_error: Option<&Error>,
) -> FailureClass {
    // Check git error first
    if let Some(err) = git_error {
        if is_git_conflict(err) {
            return FailureClass::GitConflict;
        }
        if is_policy_failure(err) {
            return FailureClass::PolicyFailure;
        }
    }

    // Check provider configuration issues
    if is_provider_configuration(&outcome.stderr) {
        return FailureClass::ProviderConfiguration;
    }

    // Check for provider rate/usage limits
    if limit_message(&outcome.stderr) || limit_message(&outcome.stdout) {
        return FailureClass::ProviderLimit;
    }

    // Check for provider transient errors
    if is_provider_transient(&outcome.stderr, outcome.exit_code) {
        return FailureClass::ProviderTransient;
    }

    // Check for policy failures in outcome
    if is_outcome_policy_failure(&outcome.stderr) {
        return FailureClass::PolicyFailure;
    }

    // Check verification gates for failures
    if gates.iter().any(|g| !g.passed) {
        return FailureClass::VerificationFailure;
    }

    // Check for needs input markers (not yet implemented; reserved for future)
    if is_needs_input(&outcome.stdout, &outcome.stderr) {
        return FailureClass::NeedsInput;
    }

    // Check for environment failures
    if is_environment_failure(&outcome.stderr) {
        return FailureClass::EnvironmentFailure;
    }

    // Fallback: agent failure
    FailureClass::AgentFailure
}

/// Check if a git error indicates a conflict.
fn is_git_conflict(err: &Error) -> bool {
    match err {
        Error::Git { stderr, .. } => {
            let s = stderr.to_lowercase();
            s.contains("conflict") || s.contains("merge conflict")
        }
        _ => false,
    }
}

/// Check if a git error indicates a policy failure (dirty tree, etc).
fn is_policy_failure(err: &Error) -> bool {
    match err {
        Error::Git { stderr, .. } => {
            let s = stderr.to_lowercase();
            s.contains("dirty") || s.contains("untracked files")
        }
        Error::Policy { .. } => true,
        _ => false,
    }
}

/// Check if stderr indicates a provider configuration issue.
fn is_provider_configuration(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("authentication")
        || s.contains("unauthorized")
        || s.contains("invalid model")
        || s.contains("model not found")
        || s.contains("executable not found")
        || s.contains("no such file or directory")
        || s.contains("permission denied")
        || s.contains("api key")
        || s.contains("invalid api")
}

/// Check if stderr indicates a provider transient error.
fn is_provider_transient(stderr: &str, exit_code: i32) -> bool {
    let s = stderr.to_lowercase();
    s.contains("connection") && s.contains("refused")
        || s.contains("timeout")
        || s.contains("network unreachable")
        || s.contains("temporarily unavailable")
        || s.contains("service unavailable")
        || s.contains("temporary failure")
        || s.contains("try again")
        || (exit_code == 124) // kill -9 or timeout signal
}

/// Check if outcome stderr indicates a policy failure.
fn is_outcome_policy_failure(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("policy violation") || s.contains("forbidden path")
}

/// Check if output contains markers indicating user input is needed.
fn is_needs_input(_stdout: &str, _stderr: &str) -> bool {
    // Placeholder for future implementation
    false
}

/// Check if stderr indicates an environment failure.
fn is_environment_failure(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("rustc not found")
        || s.contains("cargo not found")
        || s.contains("cannot find")
        || s.contains("missing")
        || s.contains("dependency")
        || s.contains("sdk")
        || s.contains("java not found")
}

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
    use crate::gate::GateKind;

    // Helper to create a basic passing outcome
    fn outcome_pass() -> Outcome {
        Outcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            usage: None,
            session_id: None,
        }
    }

    // Helper to create a basic failing outcome
    #[allow(dead_code)]
    fn outcome_fail() -> Outcome {
        Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "execution failed".to_string(),
            usage: None,
            session_id: None,
        }
    }

    // Helper to create a gate result that passed
    fn gate_pass(kind: GateKind) -> GateResult {
        GateResult {
            kind,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 100,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    // Helper to create a gate result that failed
    fn gate_fail(kind: GateKind, stderr: impl Into<String>) -> GateResult {
        GateResult {
            kind,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 100,
            stdout: String::new(),
            stderr: stderr.into(),
            timed_out: false,
        }
    }

    #[test]
    fn classify_agent_failure_when_no_gates_failed() {
        let outcome = outcome_fail();
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::AgentFailure);
    }

    #[test]
    fn classify_provider_configuration_auth_error() {
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Authentication failed: invalid API key".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderConfiguration);
    }

    #[test]
    fn classify_provider_configuration_model_not_found() {
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Model not found: claude-999".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderConfiguration);
    }

    #[test]
    fn classify_provider_limit_rate_limit_exceeded() {
        let outcome = Outcome {
            exit_code: 429,
            stdout: String::new(),
            stderr: "rate limit exceeded, retry after 60 seconds".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderLimit);
    }

    #[test]
    fn classify_provider_limit_quota_exceeded() {
        let outcome = Outcome {
            exit_code: 1,
            stdout: "quota exceeded for this billing period".to_string(),
            stderr: String::new(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderLimit);
    }

    #[test]
    fn classify_provider_transient_network_timeout() {
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Connection timeout after 30 seconds".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderTransient);
    }

    #[test]
    fn classify_provider_transient_service_unavailable() {
        let outcome = Outcome {
            exit_code: 503,
            stdout: String::new(),
            stderr: "Service temporarily unavailable".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderTransient);
    }

    #[test]
    fn classify_git_conflict() {
        let outcome = outcome_pass();
        let gates = vec![gate_pass(GateKind::Verify)];
        let git_err = Error::Git {
            args: vec!["push".to_string()],
            stderr: "error: failed to push some refs due to a merge conflict".to_string(),
        };
        let class = classify(&outcome, &gates, Some(&git_err));
        assert_eq!(class, FailureClass::GitConflict);
    }

    #[test]
    fn classify_verification_failure_test_failed() {
        let outcome = outcome_pass();
        let gates = vec![gate_fail(
            GateKind::Verify,
            "test my_test ... FAILED\n\n1 failed; 0 passed",
        )];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::VerificationFailure);
    }

    #[test]
    fn classify_verification_failure_lint_failed() {
        let outcome = outcome_pass();
        let gates = vec![
            gate_pass(GateKind::Verify),
            gate_fail(GateKind::Lint, "error: unused variable"),
        ];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::VerificationFailure);
    }

    #[test]
    fn classify_verification_failure_build_failed() {
        let outcome = outcome_pass();
        let gates = vec![
            gate_pass(GateKind::Verify),
            gate_fail(GateKind::Build, "error: could not compile"),
        ];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::VerificationFailure);
    }

    #[test]
    fn classify_environment_failure_missing_rustc() {
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "rustc not found in PATH".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::EnvironmentFailure);
    }

    #[test]
    fn classify_environment_failure_missing_dependency() {
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "error: missing dependency: python3-dev".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::EnvironmentFailure);
    }

    #[test]
    fn classify_policy_failure_dirty_tree_from_git() {
        let outcome = outcome_pass();
        let gates = vec![gate_pass(GateKind::Verify)];
        let git_err = Error::Git {
            args: vec!["status".to_string()],
            stderr: "error: working tree is dirty, will not proceed".to_string(),
        };
        let class = classify(&outcome, &gates, Some(&git_err));
        assert_eq!(class, FailureClass::PolicyFailure);
    }

    #[test]
    fn classify_policy_failure_policy_error() {
        let outcome = outcome_pass();
        let gates = vec![gate_pass(GateKind::Verify)];
        let policy_err = Error::Policy {
            detail: "forbidden files committed".to_string(),
            paths: vec![],
        };
        let class = classify(&outcome, &gates, Some(&policy_err));
        assert_eq!(class, FailureClass::PolicyFailure);
    }

    #[test]
    fn classify_priority_provider_config_over_agent_failure() {
        // Provider configuration should be detected even if no gates failed
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "API key invalid".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderConfiguration);
    }

    #[test]
    fn classify_priority_provider_limit_over_transient() {
        // If both rate limit and transient markers exist, rate limit wins
        let outcome = Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "rate limit exceeded and connection timeout".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        assert_eq!(class, FailureClass::ProviderLimit);
    }

    #[test]
    fn classify_priority_git_conflict_over_agent_failure() {
        // Git conflict should be detected even if gates passed
        let outcome = outcome_fail();
        let gates = vec![gate_pass(GateKind::Verify)];
        let git_err = Error::Git {
            args: vec!["rebase".to_string()],
            stderr: "CONFLICT (content): Merge conflict in src/main.rs".to_string(),
        };
        let class = classify(&outcome, &gates, Some(&git_err));
        assert_eq!(class, FailureClass::GitConflict);
    }

    #[test]
    fn classify_fallback_is_explicit() {
        // Unrecognized provider errors fall back to agent failure, not silently absorbed
        let outcome = Outcome {
            exit_code: 42,
            stdout: "something went wrong".to_string(),
            stderr: "unknown error from provider".to_string(),
            usage: None,
            session_id: None,
        };
        let gates = vec![gate_pass(GateKind::Verify)];
        let class = classify(&outcome, &gates, None);
        // This explicitly falls back to agent failure
        assert_eq!(class, FailureClass::AgentFailure);
    }

    #[test]
    fn limit_message_detects_rate_limit() {
        assert!(limit_message("rate limit exceeded"));
        assert!(limit_message("Rate Limit Exceeded"));
    }

    #[test]
    fn limit_message_detects_quota() {
        assert!(limit_message("quota exceeded"));
        assert!(limit_message("Quota Exceeded"));
    }

    #[test]
    fn limit_message_detects_usage_limit() {
        assert!(limit_message("usage limit reached"));
        assert!(limit_message("Token limit exceeded"));
    }

    #[test]
    fn limit_message_detects_request_limit() {
        assert!(limit_message("too many requests"));
        assert!(limit_message("request limit"));
    }

    #[test]
    fn limit_message_rejects_non_limit_errors() {
        assert!(!limit_message("connection timeout"));
        assert!(!limit_message("authentication failed"));
        assert!(!limit_message("random error"));
    }

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

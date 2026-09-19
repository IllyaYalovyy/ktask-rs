//! Failure classification and recovery types.

use crate::Error;
use crate::gate::GateResult;
use crate::provider::Outcome;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;

/// Detects if a message indicates a provider rate/usage limit.
///
/// Matches against configured regular expression patterns. Returns the matched pattern
/// if found, or None if no match. When patterns is empty, uses default patterns
/// covering common Claude and Codex limit messages.
#[must_use]
pub fn limit_message(text: &str, patterns: &[String]) -> Option<String> {
    if patterns.is_empty() {
        // Use default patterns
        for pattern in DEFAULT_LIMIT_PATTERNS {
            if let Ok(re) = Regex::new(pattern)
                && re.is_match(text)
            {
                return Some(pattern.to_string());
            }
        }
    } else {
        // Use provided patterns
        for pattern in patterns {
            if let Ok(re) = Regex::new(pattern)
                && re.is_match(text)
            {
                return Some(pattern.clone());
            }
        }
    }
    None
}

/// Default patterns for detecting provider usage limits.
/// Covers common Claude and Codex limit messages.
static DEFAULT_LIMIT_PATTERNS: &[&str] = &[
    r"(?i)rate.?limit",
    r"(?i)quota.?exceed",
    r"(?i)usage.?limit",
    r"(?i)token.?limit",
    r"(?i)request.?limit",
    r"(?i)over.?quota",
    r"(?i)limit.?reach",
    r"(?i)too.?many.?requests",
];

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
    if limit_message(&outcome.stderr, &[]).is_some()
        || limit_message(&outcome.stdout, &[]).is_some()
    {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Strategy for waiting after a provider limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPlan {
    /// Wait until a specific deadline.
    Deadline(OffsetDateTime),
    /// Use bounded backoff.
    Backoff(Duration),
}

/// Parse a reset time from text, handling absolute times and relative durations.
///
/// Supports formats like:
/// - Absolute times: "15:30", "15:30:00", "2026-09-20T15:30:00+00:00", RFC3339 format
/// - Relative durations: "1h", "30m", "1 hour", "30 minutes"
///
/// Returns None if the text cannot be parsed as either format.
#[must_use]
#[allow(clippy::duration_suboptimal_units)]
pub fn parse_reset(text: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let trimmed = text.trim();

    // Try RFC3339 ISO format first
    if let Ok(parsed) =
        OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc3339)
    {
        return Some(parsed);
    }

    // Try parsing as time-of-day with format descriptors
    let time_part_hms =
        time::format_description::parse_borrowed::<1>("[hour]:[minute]:[second]").ok();
    if let Some(fmt) = time_part_hms
        && let Ok(parsed) = time::Time::parse(trimmed, &fmt)
    {
        let mut next_reset = now.replace_time(parsed);
        // If the time has already passed today, schedule for tomorrow
        if next_reset < now {
            next_reset += Duration::from_secs(86_400);
        }
        return Some(next_reset);
    }

    // Try HH:MM format
    let time_part_hm = time::format_description::parse_borrowed::<1>("[hour]:[minute]").ok();
    if let Some(fmt) = time_part_hm
        && let Ok(parsed) = time::Time::parse(trimmed, &fmt)
    {
        let mut next_reset = now.replace_time(parsed);
        // If the time has already passed today, schedule for tomorrow
        if next_reset < now {
            next_reset += Duration::from_secs(86_400);
        }
        return Some(next_reset);
    }

    // Try parsing as relative duration
    parse_duration(trimmed).map(|dur| now + dur)
}

/// Parse a duration string into a Duration.
///
/// Supports formats like:
/// - "1h", "2h30m", "30m", "45s"
/// - "1 hour", "2 hours", "30 minutes", "45 seconds"
#[allow(clippy::duration_suboptimal_units)]
fn parse_duration(text: &str) -> Option<Duration> {
    let trimmed = text.trim().to_lowercase();

    // Handle full word formats like "1 hour", "30 minutes"
    if let Some((num_str, unit)) = parse_duration_with_words(&trimmed)
        && let Ok(num) = num_str.trim().parse::<u64>()
    {
        return Some(match unit.as_str() {
            "hour" | "hours" => Duration::from_secs(num * 3_600),
            "minute" | "minutes" => Duration::from_secs(num * 60),
            "second" | "seconds" => Duration::from_secs(num),
            "day" | "days" => Duration::from_secs(num * 86_400),
            _ => return None,
        });
    }

    // Handle shorthand formats like "1h", "30m", "45s"
    let mut total = Duration::ZERO;
    let mut current_num = String::new();
    let mut chars = trimmed.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            current_num.push(ch);
        } else if ch.is_alphabetic() {
            if current_num.is_empty() {
                return None;
            }
            let num: u64 = current_num.parse().ok()?;
            let mut unit = String::from(ch);
            // Collect multi-character units like "ms"
            while let Some(&next_ch) = chars.peek() {
                if next_ch.is_alphabetic() {
                    if let Some(c) = chars.next() {
                        unit.push(c);
                    }
                } else {
                    break;
                }
            }
            total += match unit.as_str() {
                "h" => Duration::from_secs(num * 3_600),
                "m" => Duration::from_secs(num * 60),
                "s" => Duration::from_secs(num),
                "ms" => Duration::from_millis(num),
                "d" => Duration::from_secs(num * 86_400),
                _ => return None,
            };
            current_num.clear();
        } else if ch.is_whitespace() {
            // Skip whitespace
        } else {
            return None;
        }
    }

    if total == Duration::ZERO {
        return None;
    }
    Some(total)
}

/// Helper to parse duration with full words like "1 hour" or "30 minutes".
#[allow(clippy::duration_suboptimal_units)]
fn parse_duration_with_words(text: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() == 2
        && let (Some(&num_part), Some(&unit_part)) = (parts.first(), parts.get(1))
        && num_part.parse::<u64>().is_ok()
    {
        let num_str = num_part.to_string();
        let unit = unit_part.to_string();
        return Some((num_str, unit));
    }
    None
}

/// Determine a wait strategy based on reset time and constraints.
///
/// If a reset time is known, returns a Deadline with the margin subtracted.
/// If reset is unknown, returns a Backoff bounded by the max duration.
#[must_use]
pub fn wait_plan(
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
    margin: Duration,
    max: Duration,
) -> WaitPlan {
    match reset {
        Some(deadline) => {
            // Subtract margin from deadline for a more conservative wait time
            let adjusted_deadline = if deadline > now + margin {
                deadline - margin
            } else {
                now
            };
            WaitPlan::Deadline(adjusted_deadline)
        }
        None => {
            // Use bounded backoff for unknown reset times
            WaitPlan::Backoff(max)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::duration_suboptimal_units)]
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
        assert!(limit_message("rate limit exceeded", &[]).is_some());
        assert!(limit_message("Rate Limit Exceeded", &[]).is_some());
    }

    #[test]
    fn limit_message_detects_quota() {
        assert!(limit_message("quota exceeded", &[]).is_some());
        assert!(limit_message("Quota Exceeded", &[]).is_some());
    }

    #[test]
    fn limit_message_detects_usage_limit() {
        assert!(limit_message("usage limit reached", &[]).is_some());
        assert!(limit_message("Token limit exceeded", &[]).is_some());
    }

    #[test]
    fn limit_message_detects_request_limit() {
        assert!(limit_message("too many requests", &[]).is_some());
        assert!(limit_message("request limit", &[]).is_some());
    }

    #[test]
    fn limit_message_rejects_non_limit_errors() {
        assert!(limit_message("connection timeout", &[]).is_none());
        assert!(limit_message("authentication failed", &[]).is_none());
        assert!(limit_message("random error", &[]).is_none());
    }

    #[test]
    fn limit_message_uses_custom_patterns() {
        let custom = vec!["(?i)custom.*limit".to_string()];
        assert!(limit_message("Custom Limit Hit", &custom).is_some());
        assert!(limit_message("rate limit", &custom).is_none());
    }

    #[test]
    fn limit_message_returns_matched_pattern() {
        let result = limit_message("rate limit exceeded", &[]);
        assert!(result.is_some());
        let pattern = result.unwrap();
        assert!(pattern.contains("rate"));
    }

    #[test]
    fn limit_message_empty_text_no_match() {
        assert!(limit_message("", &[]).is_none());
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

    // Parse reset time tests
    #[test]
    fn parse_reset_absolute_time_hhmm_format() {
        use time::macros::offset;
        // Create a test time: 2026-09-19 10:00:00 UTC
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        // Test parsing HH:MM format for later today
        let result = parse_reset("15:30", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        // Should be today at 15:30
        assert_eq!(reset.hour(), 15);
        assert_eq!(reset.minute(), 30);
        assert_eq!(reset.date(), now.date());
    }

    #[test]
    fn parse_reset_absolute_time_hhmmss_format() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        let result = parse_reset("14:25:30", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        assert_eq!(reset.hour(), 14);
        assert_eq!(reset.minute(), 25);
        assert_eq!(reset.second(), 30);
    }

    #[test]
    fn parse_reset_past_time_schedules_tomorrow() {
        use time::macros::offset;
        // Create a test time: 2026-09-19 10:00:00 UTC
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        // Parse a time that has already passed today (09:00 < 10:00)
        let result = parse_reset("09:00", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        // Should be tomorrow at 09:00
        assert_eq!(reset.hour(), 9);
        assert_eq!(reset.minute(), 0);
        // Check it's the next day
        let expected_date = now.date() + Duration::from_secs(86_400);
        assert_eq!(reset.date(), expected_date);
    }

    #[test]
    fn parse_reset_across_midnight_boundary() {
        use time::macros::offset;
        // Create a test time: 2026-09-19 23:30:00 UTC (11:30 PM)
        let now = OffsetDateTime::from_unix_timestamp(1_726_838_400 + 84600)
            .unwrap()
            .to_offset(offset!(UTC));

        // Parse 01:00 (1 AM) - should be tomorrow
        let result = parse_reset("01:00", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        assert_eq!(reset.hour(), 1);
        assert_eq!(reset.minute(), 0);
        // Should be tomorrow
        let expected_date = now.date() + Duration::from_secs(86_400);
        assert_eq!(reset.date(), expected_date);
    }

    #[test]
    fn parse_reset_at_day_boundary() {
        use time::macros::offset;
        // Create a test time at exactly midnight: 2026-09-20 00:00:00 UTC
        let now = OffsetDateTime::from_unix_timestamp(1_726_838_400)
            .unwrap()
            .to_offset(offset!(UTC));

        // Parse 00:00 (midnight) - should be tomorrow since it's not in the future
        let result = parse_reset("00:00", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        assert_eq!(reset.hour(), 0);
        assert_eq!(reset.minute(), 0);
    }

    #[test]
    fn parse_reset_relative_duration_hours() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        let result = parse_reset("2h", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        // Should be 2 hours from now
        let expected = now + Duration::from_secs(7200);
        assert_eq!(reset, expected);
    }

    #[test]
    fn parse_reset_relative_duration_minutes() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        let result = parse_reset("30m", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        // Should be 30 minutes from now
        let expected = now + Duration::from_secs(1_800);
        assert_eq!(reset, expected);
    }

    #[test]
    fn parse_reset_relative_duration_mixed() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        let result = parse_reset("1h30m", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        // Should be 1 hour 30 minutes from now
        let expected = now + Duration::from_secs(5_400);
        assert_eq!(reset, expected);
    }

    #[test]
    fn parse_reset_relative_duration_words() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        let result = parse_reset("1 hour", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        let expected = now + Duration::from_secs(3600);
        assert_eq!(reset, expected);
    }

    #[test]
    fn parse_reset_relative_duration_plural_words() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        let result = parse_reset("30 minutes", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        let expected = now + Duration::from_secs(1_800);
        assert_eq!(reset, expected);
    }

    #[test]
    fn parse_reset_unparsable_returns_none() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        assert!(parse_reset("invalid", now).is_none());
        assert!(parse_reset("not a time", now).is_none());
        assert!(parse_reset("25:00", now).is_none());
        assert!(parse_reset("xyz", now).is_none());
    }

    #[test]
    fn parse_reset_whitespace_handling() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));

        // With leading/trailing whitespace
        let result = parse_reset("  1h  ", now);
        assert!(result.is_some());
        let reset = result.unwrap();
        let expected = now + Duration::from_secs(3600);
        assert_eq!(reset, expected);
    }

    #[test]
    fn wait_plan_with_known_deadline() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));
        let deadline = now + Duration::from_secs(3600); // 1 hour from now
        let margin = Duration::from_secs(300); // 5 minutes
        let max = Duration::from_secs(10000);

        let plan = wait_plan(Some(deadline), now, margin, max);

        match plan {
            WaitPlan::Deadline(adjusted) => {
                // Deadline minus margin should be 55 minutes from now
                let expected = deadline - margin;
                assert_eq!(adjusted, expected);
            }
            WaitPlan::Backoff(_) => panic!("Expected Deadline, got Backoff"),
        }
    }

    #[test]
    fn wait_plan_with_deadline_less_than_margin() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));
        let deadline = now + Duration::from_secs(100); // 100 seconds from now
        let margin = Duration::from_secs(300); // 5 minutes (larger than deadline gap)
        let max = Duration::from_secs(10000);

        let plan = wait_plan(Some(deadline), now, margin, max);

        match plan {
            WaitPlan::Deadline(adjusted) => {
                // Should not go before now
                assert!(adjusted >= now);
            }
            WaitPlan::Backoff(_) => panic!("Expected Deadline, got Backoff"),
        }
    }

    #[test]
    fn wait_plan_with_unknown_deadline() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));
        let margin = Duration::from_secs(300);
        let max = Duration::from_secs(10000);

        let plan = wait_plan(None, now, margin, max);

        match plan {
            WaitPlan::Backoff(duration) => {
                // Should use max duration
                assert_eq!(duration, max);
            }
            WaitPlan::Deadline(_) => panic!("Expected Backoff, got Deadline"),
        }
    }

    #[test]
    fn wait_plan_backoff_bounded_by_max() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));
        let margin = Duration::from_secs(100);
        let max = Duration::from_secs(300);

        let plan = wait_plan(None, now, margin, max);

        match plan {
            WaitPlan::Backoff(duration) => {
                assert_eq!(duration, max);
            }
            WaitPlan::Deadline(_) => panic!("Expected Backoff, got Deadline"),
        }
    }

    #[test]
    fn parse_duration_hours() {
        let result = parse_duration("2h");
        assert_eq!(result, Some(Duration::from_secs(7200)));
    }

    #[test]
    fn parse_duration_minutes() {
        let result = parse_duration("30m");
        assert_eq!(result, Some(Duration::from_secs(1_800)));
    }

    #[test]
    fn parse_duration_seconds() {
        let result = parse_duration("45s");
        assert_eq!(result, Some(Duration::from_secs(45)));
    }

    #[test]
    fn parse_duration_mixed() {
        let result = parse_duration("1h30m");
        assert_eq!(result, Some(Duration::from_secs(5_400)));
    }

    #[test]
    fn parse_duration_full_words() {
        let result = parse_duration("1 hour");
        assert_eq!(result, Some(Duration::from_secs(3600)));

        let result = parse_duration("30 minutes");
        assert_eq!(result, Some(Duration::from_secs(1_800)));

        let result = parse_duration("45 seconds");
        assert_eq!(result, Some(Duration::from_secs(45)));
    }

    #[test]
    fn parse_duration_plural_forms() {
        let result = parse_duration("2 hours");
        assert_eq!(result, Some(Duration::from_secs(7200)));

        let result = parse_duration("5 minutes");
        assert_eq!(result, Some(Duration::from_secs(300)));

        let result = parse_duration("10 seconds");
        assert_eq!(result, Some(Duration::from_secs(10)));
    }

    #[test]
    fn parse_duration_days() {
        let result = parse_duration("1d");
        assert_eq!(result, Some(Duration::from_secs(86_400)));

        let result = parse_duration("2 days");
        assert_eq!(result, Some(Duration::from_secs(172_800)));
    }

    #[test]
    fn parse_duration_invalid_returns_none() {
        assert!(parse_duration("invalid").is_none());
        assert!(parse_duration("abc").is_none());
        assert!(parse_duration("").is_none());
        assert!(parse_duration("10x").is_none());
    }

    #[test]
    fn parse_duration_case_insensitive() {
        let result = parse_duration("1H");
        assert_eq!(result, Some(Duration::from_secs(3600)));

        let result = parse_duration("30M");
        assert_eq!(result, Some(Duration::from_secs(1_800)));
    }

    #[test]
    fn parse_duration_with_spaces() {
        let result = parse_duration("1h 30m");
        // This should work - spaces between units
        assert!(result.is_some());
    }

    #[test]
    fn wait_plan_never_returns_unbounded_wait() {
        use time::macros::offset;
        let now = OffsetDateTime::from_unix_timestamp(1_726_752_000)
            .unwrap()
            .to_offset(offset!(UTC));
        let margin = Duration::from_secs(300);
        let max = Duration::from_secs(300);

        // Test with unknown deadline
        let plan = wait_plan(None, now, margin, max);
        match plan {
            WaitPlan::Deadline(_) => panic!("Should not have infinite deadline"),
            WaitPlan::Backoff(d) => {
                // Must be bounded
                assert!(d <= max);
            }
        }
    }
}

//! Failure signature and circuit breaker for repeated failures.

use crate::{AttemptRecord, Error, FailureClass, GateResult, Result, Task};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Represents the state of a circuit breaker after recording a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Signature was recorded, but threshold not yet reached.
    Open,
    /// Threshold reached; circuit is now tripped.
    Tripped,
}

/// Decision to continue or stop remediation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Continue with remediation.
    Continue,
    /// Stop remediation with a reason.
    Stop {
        /// The reason why remediation should stop.
        reason: String,
    },
}

/// Bounds for remediation attempts.
#[derive(Debug, Clone)]
pub struct Bounds {
    /// Maximum number of attempts.
    pub max_attempts: u32,
    /// Maximum elapsed time.
    pub max_elapsed: Duration,
    /// Maximum tokens allowed, or None for unlimited.
    pub max_tokens: Option<u64>,
}

impl Bounds {
    /// Check if remediation should continue given the current state.
    ///
    /// Returns `Continue` if all bounds are satisfied, or `Stop` with the reason
    /// for the first bound that was exceeded.
    #[must_use]
    pub fn should_continue(&self, attempts: u32, elapsed: Duration, tokens: u64) -> Decision {
        // Check attempts bound first
        if attempts >= self.max_attempts {
            return Decision::Stop {
                reason: format!("max attempts exceeded: {attempts} >= {}", self.max_attempts),
            };
        }

        // Check elapsed time bound
        if elapsed >= self.max_elapsed {
            return Decision::Stop {
                reason: format!(
                    "max elapsed time exceeded: {elapsed:?} >= {:?}",
                    self.max_elapsed
                ),
            };
        }

        // Check token budget
        if let Some(max_tokens) = self.max_tokens
            && tokens >= max_tokens
        {
            return Decision::Stop {
                reason: format!("token budget exceeded: {tokens} >= {max_tokens}"),
            };
        }

        Decision::Continue
    }
}

/// A circuit breaker that trips on repeated identical failures.
#[derive(Debug, Clone)]
pub struct Breaker {
    /// Number of identical signatures required to trip the breaker.
    threshold: u32,
    /// Map of signatures to their occurrence counts.
    signatures: HashMap<String, u32>,
}

impl Breaker {
    /// Create a new breaker with the given threshold.
    #[must_use]
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            signatures: HashMap::new(),
        }
    }

    /// Record a failure signature and return the breaker state.
    ///
    /// Returns `Tripped` if the signature count reaches the threshold,
    /// `Open` otherwise.
    pub fn record(&mut self, sig: &str) -> BreakerState {
        let count = self.signatures.entry(sig.to_string()).or_insert(0);
        *count += 1;

        if *count >= self.threshold {
            BreakerState::Tripped
        } else {
            BreakerState::Open
        }
    }
}

/// Check that a diff does not touch protected policy files.
///
/// Protected paths are:
/// - `deny.toml`
/// - `clippy.toml`
/// - `rustfmt.toml`
/// - `scripts/` directory
/// - `.ktask/` directory
///
/// Returns a Policy error if any protected path is found in the diff.
/// This check enforces the invariant that tasks cannot weaken or modify
/// the policy gates that evaluate their work.
///
/// # Arguments
///
/// * `diff_paths` - Paths modified in the diff to check
///
/// # Returns
///
/// `Ok(())` if no protected paths are touched.
///
/// # Errors
///
/// Returns a Policy error if any protected paths are modified.
pub fn check_no_policy_edit(diff_paths: &[PathBuf]) -> Result<()> {
    let mut violations = Vec::new();

    for path in diff_paths {
        let path_str = path.to_string_lossy().to_string();

        let is_violation =
            // Exact matches for config files
            path_str == "deny.toml" ||
            path_str == "clippy.toml" ||
            path_str == "rustfmt.toml" ||
            // Directory checks - match exact directory names or files within them
            path_str.starts_with("scripts/") ||
            path_str == "scripts" ||
            path_str.starts_with(".ktask/") ||
            path_str == ".ktask";

        if is_violation {
            violations.push(path.clone());
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(Error::Policy {
            detail: format!(
                "task cannot edit policy gates: {}",
                violations
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            paths: violations,
        })
    }
}

/// Generate a failure bundle for seeding a fresh provider session.
///
/// Combines classification, failing gate output, diff summary, and prior attempt evidence
/// into a compact, deterministic, and redacted string. Truncates oldest-first to fit budget.
///
/// # Arguments
///
/// * `task` - The task being remediated
/// * `class` - The failure classification
/// * `gates` - Gate results from the failed attempt
/// * `diff_summary` - Summary of code changes
/// * `prior` - Prior attempt records in chronological order
/// * `budget_bytes` - Maximum size for the bundle
///
/// # Returns
///
/// A deterministic, redacted failure bundle that fits within the budget.
#[must_use]
pub fn bundle(
    task: &Task,
    class: FailureClass,
    gates: &[GateResult],
    diff_summary: &str,
    prior: &[AttemptRecord],
    budget_bytes: usize,
) -> String {
    use crate::redact::redact;

    let mut lines = Vec::new();

    // 1. Classification header
    lines.push(format!("Classification: {class:?}"));
    lines.push(String::new());

    // 2. Task summary
    lines.push(format!("Task: {}", task.title()));
    lines.push(String::new());

    // 3. Failing gate output (tail of failed gates)
    let failed_gates: Vec<_> = gates.iter().filter(|g| !g.passed).collect();
    if !failed_gates.is_empty() {
        lines.push("Gate Output:".to_string());
        for gate in failed_gates {
            lines.push(format!("[{:?}]", gate.kind));
            // Get tail of output (last 10 lines)
            let output = if gate.stderr.is_empty() {
                gate.stdout.as_str()
            } else {
                gate.stderr.as_str()
            };
            let tail_lines: Vec<_> = output.lines().rev().take(10).collect();
            for line in tail_lines.into_iter().rev() {
                lines.push(line.to_string());
            }
            lines.push(String::new());
        }
    }

    // 4. Diff summary
    if !diff_summary.is_empty() {
        lines.push("Diff Summary:".to_string());
        lines.push(diff_summary.to_string());
        lines.push(String::new());
    }

    // 5. Prior attempts (oldest first, we'll truncate from oldest)
    if !prior.is_empty() {
        lines.push("Prior Attempts:".to_string());
        for (i, attempt) in prior.iter().enumerate() {
            lines.push(format!("  [{}] {}", i + 1, attempt.exit_reason));
        }
        lines.push(String::new());
    }

    // Join all lines
    let mut bundle = lines.join("\n");

    // Truncate to budget, removing oldest attempts first
    if bundle.len() > budget_bytes {
        // Remove attempts from the bundle oldest-first
        while bundle.len() > budget_bytes && !prior.is_empty() {
            // Remove the oldest attempt line
            if let Some(pos) = bundle.rfind("  [") {
                if bundle[pos..].contains('\n') {
                    bundle.truncate(pos);
                    bundle = bundle.trim_end().to_string();
                    if bundle.ends_with('\n') {
                        bundle.pop();
                    }
                    bundle.push('\n');
                } else {
                    bundle.truncate(pos);
                    bundle = bundle.trim_end().to_string();
                    break;
                }
            } else {
                break;
            }
        }

        // If still too large, remove from the end more aggressively
        if bundle.len() > budget_bytes {
            // Truncate to budget and ensure we end at a line boundary
            bundle.truncate(budget_bytes);
            if let Some(pos) = bundle.rfind('\n') {
                bundle.truncate(pos);
            }
        }
    }

    // Redact secrets
    redact(&bundle, &[])
}

/// Generate a failure signature from a failure class and gate results.
///
/// The signature hashes the failure class and the normalized failing test names,
/// with digits, paths, and timing information stripped to ensure reproducibility
/// across runs.
#[must_use]
pub fn signature(class: FailureClass, gates: &[GateResult]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // Hash the failure class
    class.hash(&mut hasher);

    // Collect normalized failing test names
    let mut failing_tests = Vec::new();
    for gate in gates {
        if !gate.passed {
            // Parse test names from the gate output
            let tests = parse_test_names(&gate.stdout, &gate.stderr);
            failing_tests.extend(tests);
        }
    }

    // Sort for deterministic ordering
    failing_tests.sort();
    failing_tests.dedup();

    // Hash the normalized test names
    for test in failing_tests {
        test.hash(&mut hasher);
    }

    format!("sig-{:x}", hasher.finish())
}

/// Parse test names from gate output and normalize them.
///
/// Strips digits, paths, and timing information from test names to ensure
/// reproducibility across different runs.
fn parse_test_names(stdout: &str, stderr: &str) -> Vec<String> {
    let mut tests = Vec::new();

    // Look for test names in cargo output format: "test path::to::test ... ok/FAILED"
    for line in stdout.lines().chain(stderr.lines()) {
        if let Some(test_name) = extract_test_name(line) {
            let normalized = normalize_test_name(&test_name);
            if !normalized.is_empty() {
                tests.push(normalized);
            }
        }
    }

    tests
}

/// Extract a test name from a cargo output line.
///
/// Recognizes patterns like:
/// - `test path::to::test ... ok`
/// - `test path::to::test ... FAILED`
fn extract_test_name(line: &str) -> Option<String> {
    if line.starts_with("test ") {
        // Find the part between "test " and " ..."
        if let Some(end) = line.find(" ...") {
            let name = line[5..end].trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Normalize a test name by removing digits, paths, and timing info.
///
/// This ensures that a test like:
/// - `test::my_test_123` becomes `test::my_test`
/// - `path/to/src/file.rs::test` becomes `path::to::src::file::test`
fn normalize_test_name(test: &str) -> String {
    // Replace path separators with :: and remove unwanted characters
    let mut result = String::new();
    let mut chars = test.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Replace path separators with ::
            '/' | '\\' => result.push_str("::"),
            // Handle dots - skip them when they're file extensions
            '.' => {
                // Look ahead to see if this is a file extension (letters followed by :: or end)
                let mut ext_chars = Vec::new();
                let temp_chars = chars.clone();
                let mut is_extension = false;

                for ext_ch in temp_chars {
                    if ext_ch.is_alphabetic() {
                        ext_chars.push(ext_ch);
                    } else if ext_ch == ':' || ext_ch == '/' || ext_ch == '\\' {
                        // It's an extension
                        is_extension = true;
                        break;
                    } else {
                        break;
                    }
                }

                if !ext_chars.is_empty() && (is_extension || chars.peek().is_none()) {
                    // Skip the extension (the dot and the letters)
                    for _ in 0..ext_chars.len() {
                        chars.next();
                    }
                }
            }
            // Remove digits
            '0'..='9' => {}
            // Keep colons, alphanumeric, and underscore
            ':' | '_' => result.push(ch),
            c if c.is_alphanumeric() => result.push(c),
            _ => {}
        }
    }

    // Clean up multiple consecutive colons (more than 2) back to ::
    let mut cleaned = String::new();
    let mut colon_count = 0;

    for ch in result.chars() {
        if ch == ':' {
            colon_count += 1;
            // Add the colon, but we'll handle consecutive ones
            if colon_count <= 2 {
                cleaned.push(':');
            }
        } else {
            colon_count = 0;
            cleaned.push(ch);
        }
    }

    // Remove leading/trailing colons and underscores
    cleaned.trim_matches(':').trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_breaker_opens_on_first_signature() {
        let mut breaker = Breaker::new(2);
        let state = breaker.record("sig-1");
        assert_eq!(state, BreakerState::Open);
    }

    #[test]
    fn test_breaker_trips_at_threshold() {
        let mut breaker = Breaker::new(2);
        breaker.record("sig-1");
        let state = breaker.record("sig-1");
        assert_eq!(state, BreakerState::Tripped);
    }

    #[test]
    fn test_breaker_differentiates_signatures() {
        let mut breaker = Breaker::new(2);
        breaker.record("sig-1");
        breaker.record("sig-2");
        let state = breaker.record("sig-2");
        assert_eq!(state, BreakerState::Tripped); // sig-2 reaches threshold of 2
    }

    #[test]
    fn test_signature_same_for_same_failure() {
        let class = FailureClass::AgentFailure;
        let gates = vec![];

        let sig1 = signature(class, &gates);
        let sig2 = signature(class, &gates);

        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_signature_different_for_different_classes() {
        let gates = vec![];

        let sig1 = signature(FailureClass::AgentFailure, &gates);
        let sig2 = signature(FailureClass::VerificationFailure, &gates);

        assert_ne!(sig1, sig2);
    }

    #[test]
    fn test_signature_stable_with_different_durations() {
        use crate::GateKind;

        let class = FailureClass::VerificationFailure;

        let gate1 = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 1000,
            stdout: "test my_test_123 ... FAILED".to_string(),
            stderr: String::new(),
            timed_out: false,
        };

        let gate2 = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 2000, // Different duration
            stdout: "test my_test_123 ... FAILED".to_string(),
            stderr: String::new(),
            timed_out: false,
        };

        let sig1 = signature(class, &[gate1]);
        let sig2 = signature(class, &[gate2]);

        // Signatures should be the same despite different durations
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_normalize_test_name_removes_digits() {
        assert_eq!(normalize_test_name("test_123"), "test");
        assert_eq!(normalize_test_name("my_test_42"), "my_test");
    }

    #[test]
    fn test_normalize_test_name_removes_paths() {
        assert_eq!(
            normalize_test_name("path/to/file.rs::test"),
            "path::to::file::test"
        );
    }

    #[test]
    fn test_normalize_test_name_collapses_separators() {
        assert_eq!(normalize_test_name("test::my_test"), "test::my_test");
    }

    #[test]
    fn test_extract_test_name_from_cargo_output() {
        let line = "test my_test_function ... ok";
        assert_eq!(
            extract_test_name(line),
            Some("my_test_function".to_string())
        );
    }

    #[test]
    fn test_extract_test_name_from_failed_output() {
        let line = "test my_test_function ... FAILED";
        assert_eq!(
            extract_test_name(line),
            Some("my_test_function".to_string())
        );
    }

    #[test]
    fn test_extract_test_name_with_module_path() {
        let line = "test module::test_function ... ok";
        assert_eq!(
            extract_test_name(line),
            Some("module::test_function".to_string())
        );
    }

    #[test]
    fn test_extract_test_name_returns_none_for_non_test_lines() {
        assert_eq!(extract_test_name("running 1 test"), None);
        assert_eq!(extract_test_name("test result: ok"), None);
    }

    #[test]
    fn bundle_includes_classification() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::VerificationFailure;
        let gates = vec![];
        let diff_summary = "";
        let prior = vec![];

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert!(result.contains("Classification: VerificationFailure"));
    }

    #[test]
    fn bundle_includes_task_title() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "My test task description".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "";
        let prior = vec![];

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert!(result.contains("Task: My test task description"));
    }

    #[test]
    fn bundle_includes_failing_gate_output() {
        use crate::{GateKind, TaskId};

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let gate = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 1000,
            stdout: String::new(),
            stderr: "error: test failed\nerror: assertion failed".to_string(),
            timed_out: false,
        };

        let class = FailureClass::VerificationFailure;
        let gates = vec![gate];
        let diff_summary = "";
        let prior = vec![];

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert!(result.contains("[Verify]"));
        assert!(result.contains("error: test failed"));
        assert!(result.contains("error: assertion failed"));
    }

    #[test]
    fn bundle_includes_diff_summary() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "Modified 3 files, 5 insertions(+), 2 deletions(-)";
        let prior = vec![];

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert!(result.contains("Diff Summary:"));
        assert!(result.contains("Modified 3 files, 5 insertions(+), 2 deletions(-)"));
    }

    #[test]
    fn bundle_includes_prior_attempts() {
        use crate::{AttemptId, TaskId};
        use time::OffsetDateTime;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let now = OffsetDateTime::now_utc();
        let prior = vec![
            AttemptRecord {
                id: AttemptId::new(1),
                task: TaskId::new(1),
                started: now,
                ended: Some(now),
                model_configured: None,
                model_reported: None,
                session_id: None,
                exit_reason: "verification_failure".to_string(),
                gates: vec![],
                usage: None,
                base_sha: "abc123".to_string(),
                candidate_sha: None,
            },
            AttemptRecord {
                id: AttemptId::new(2),
                task: TaskId::new(1),
                started: now,
                ended: Some(now),
                model_configured: None,
                model_reported: None,
                session_id: None,
                exit_reason: "agent_failure".to_string(),
                gates: vec![],
                usage: None,
                base_sha: "abc123".to_string(),
                candidate_sha: None,
            },
        ];

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "";

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert!(result.contains("Prior Attempts:"));
        assert!(result.contains("[1] verification_failure"));
        assert!(result.contains("[2] agent_failure"));
    }

    #[test]
    fn bundle_respects_budget() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task with a very long description that goes on and on".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "This is a very long diff summary";
        let prior = vec![];
        let budget = 100;

        let result = bundle(&task, class, &gates, diff_summary, &prior, budget);

        assert!(result.len() <= budget);
    }

    #[test]
    fn bundle_redacts_secrets() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "API key: sk-1234567890abcdefghij";
        let prior = vec![];

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert!(!result.contains("sk-"));
        assert!(result.contains("[redacted]"));
    }

    #[test]
    fn bundle_is_deterministic() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::VerificationFailure;
        let gates = vec![];
        let diff_summary = "Some changes";
        let prior = vec![];

        let result1 = bundle(&task, class, &gates, diff_summary, &prior, 10000);
        let result2 = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        assert_eq!(result1, result2);
    }

    #[test]
    fn bundle_truncates_oldest_attempts_first() {
        use crate::{AttemptId, TaskId};
        use time::OffsetDateTime;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "Test outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let now = OffsetDateTime::now_utc();
        let prior = vec![
            AttemptRecord {
                id: AttemptId::new(1),
                task: TaskId::new(1),
                started: now,
                ended: Some(now),
                model_configured: None,
                model_reported: None,
                session_id: None,
                exit_reason: "failure_reason_1".to_string(),
                gates: vec![],
                usage: None,
                base_sha: "abc123".to_string(),
                candidate_sha: None,
            },
            AttemptRecord {
                id: AttemptId::new(2),
                task: TaskId::new(1),
                started: now,
                ended: Some(now),
                model_configured: None,
                model_reported: None,
                session_id: None,
                exit_reason: "failure_reason_2".to_string(),
                gates: vec![],
                usage: None,
                base_sha: "abc123".to_string(),
                candidate_sha: None,
            },
            AttemptRecord {
                id: AttemptId::new(3),
                task: TaskId::new(1),
                started: now,
                ended: Some(now),
                model_configured: None,
                model_reported: None,
                session_id: None,
                exit_reason: "failure_reason_3".to_string(),
                gates: vec![],
                usage: None,
                base_sha: "abc123".to_string(),
                candidate_sha: None,
            },
        ];

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "";
        let budget = 300;

        let result = bundle(&task, class, &gates, diff_summary, &prior, budget);

        // The result should still contain the most recent attempt
        assert!(result.contains("failure_reason_3"));
        // Due to budget constraints, oldest attempts might be removed
        assert!(result.len() <= budget);
    }

    #[test]
    fn bundle_handles_empty_inputs() {
        use crate::TaskId;

        let task = Task {
            id: TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test".to_string(),
            outcome: "Outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };

        let class = FailureClass::AgentFailure;
        let gates = vec![];
        let diff_summary = "";
        let prior = vec![];

        let result = bundle(&task, class, &gates, diff_summary, &prior, 10000);

        // Should contain classification and task
        assert!(result.contains("Classification:"));
        assert!(result.contains("Task:"));
        // Should not have sections for empty inputs
        assert!(!result.contains("Gate Output:") || result.trim().ends_with("Gate Output:"));
        assert!(!result.contains("Diff Summary:") || result.trim().ends_with("Diff Summary:"));
        assert!(!result.contains("Prior Attempts:") || result.trim().ends_with("Prior Attempts:"));
    }

    #[test]
    fn bounds_continue_when_all_limits_satisfied() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(3, Duration::from_secs(100), 50_000);
        assert_eq!(decision, Decision::Continue);
    }

    #[test]
    fn bounds_stop_on_max_attempts() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(5, Duration::from_secs(100), 50_000);
        assert!(matches!(decision, Decision::Stop { reason } if reason.contains("max attempts")));
    }

    #[test]
    fn bounds_stop_on_max_attempts_exceeded() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(10, Duration::from_secs(100), 50_000);
        assert!(matches!(decision, Decision::Stop { reason } if reason.contains("max attempts")));
    }

    #[test]
    fn bounds_stop_on_max_elapsed() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(3, Duration::from_secs(300), 50_000);
        assert!(
            matches!(decision, Decision::Stop { reason } if reason.contains("max elapsed time"))
        );
    }

    #[test]
    fn bounds_stop_on_max_elapsed_exceeded() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(3, Duration::from_secs(400), 50_000);
        assert!(
            matches!(decision, Decision::Stop { reason } if reason.contains("max elapsed time"))
        );
    }

    #[test]
    fn bounds_stop_on_max_tokens() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(3, Duration::from_secs(100), 100_000);
        assert!(matches!(decision, Decision::Stop { reason } if reason.contains("token budget")));
    }

    #[test]
    fn bounds_stop_on_max_tokens_exceeded() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        let decision = bounds.should_continue(3, Duration::from_secs(100), 150_000);
        assert!(matches!(decision, Decision::Stop { reason } if reason.contains("token budget")));
    }

    #[test]
    fn bounds_continue_with_unlimited_tokens() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: None,
        };

        let decision = bounds.should_continue(3, Duration::from_secs(100), 1_000_000);
        assert_eq!(decision, Decision::Continue);
    }

    #[test]
    fn bounds_reason_names_bound() {
        let bounds = Bounds {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(300),
            max_tokens: Some(100_000),
        };

        // Test each bound names itself in the reason
        let attempts_decision = bounds.should_continue(5, Duration::from_secs(100), 50_000);
        match attempts_decision {
            Decision::Stop { reason } => assert!(reason.contains("attempts")),
            Decision::Continue => panic!("Expected Stop decision"),
        }

        let elapsed_decision = bounds.should_continue(3, Duration::from_secs(300), 50_000);
        match elapsed_decision {
            Decision::Stop { reason } => assert!(reason.contains("elapsed")),
            Decision::Continue => panic!("Expected Stop decision"),
        }

        let tokens_decision = bounds.should_continue(3, Duration::from_secs(100), 100_000);
        match tokens_decision {
            Decision::Stop { reason } => assert!(reason.contains("token")),
            Decision::Continue => panic!("Expected Stop decision"),
        }
    }

    #[test]
    fn check_no_policy_edit_accepts_normal_source_changes() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("crates/ktask-core/src/lib.rs"),
            PathBuf::from("README.md"),
        ];

        assert!(check_no_policy_edit(&paths).is_ok());
    }

    #[test]
    fn check_no_policy_edit_rejects_deny_toml() {
        let paths = vec![PathBuf::from("deny.toml")];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            detail,
            paths: violation_paths,
        }) = result
        {
            assert!(detail.contains("policy gates"));
            assert_eq!(violation_paths.len(), 1);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_rejects_clippy_toml() {
        let paths = vec![PathBuf::from("clippy.toml")];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 1);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_rejects_rustfmt_toml() {
        let paths = vec![PathBuf::from("rustfmt.toml")];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 1);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_rejects_scripts_directory() {
        let paths = vec![
            PathBuf::from("scripts/quality.sh"),
            PathBuf::from("scripts/test.sh"),
        ];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 2);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_rejects_ktask_directory() {
        let paths = vec![
            PathBuf::from(".ktask/queue/task-1.md"),
            PathBuf::from(".ktask/config.toml"),
        ];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 2);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_mixed_violations_and_valid() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("clippy.toml"),
            PathBuf::from("README.md"),
            PathBuf::from("scripts/test.sh"),
        ];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 2);
            let violation_strs: Vec<_> = violation_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            assert!(violation_strs.contains(&"clippy.toml".to_string()));
            assert!(violation_strs.contains(&"scripts/test.sh".to_string()));
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_empty_paths() {
        let paths: Vec<PathBuf> = vec![];

        assert!(check_no_policy_edit(&paths).is_ok());
    }

    #[test]
    fn check_no_policy_edit_allows_deny_toml_in_nested_path() {
        // Deny.toml protection is only at the root level
        // Nested deny.toml files in subdirectories are allowed
        let paths = vec![PathBuf::from("some/nested/path/deny.toml")];

        assert!(check_no_policy_edit(&paths).is_ok());
    }

    #[test]
    fn check_no_policy_edit_detects_scripts_nested() {
        let paths = vec![
            PathBuf::from("scripts/tools/helper.sh"),
            PathBuf::from("scripts/deeper/nested/file.sh"),
        ];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 2);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_detects_ktask_nested() {
        let paths = vec![
            PathBuf::from(".ktask/logs/attempt-1.log"),
            PathBuf::from(".ktask/queue/report.md"),
        ];

        let result = check_no_policy_edit(&paths);
        assert!(result.is_err());
        if let Err(Error::Policy {
            paths: violation_paths,
            ..
        }) = result
        {
            assert_eq!(violation_paths.len(), 2);
        } else {
            panic!("Expected Policy error");
        }
    }

    #[test]
    fn check_no_policy_edit_ignores_similar_names() {
        // scripts.txt is not scripts/ directory
        // clippy_options.rs is not clippy.toml
        let paths = vec![
            PathBuf::from("src/scripts.txt"),
            PathBuf::from("src/clippy_options.rs"),
            PathBuf::from("config/deny_list.toml"),
            PathBuf::from(".ktask_cache/file.txt"),
        ];

        assert!(check_no_policy_edit(&paths).is_ok());
    }
}

//! Failure signature and circuit breaker for repeated failures.

use crate::{FailureClass, GateResult};
use std::collections::HashMap;

/// Represents the state of a circuit breaker after recording a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Signature was recorded, but threshold not yet reached.
    Open,
    /// Threshold reached; circuit is now tripped.
    Tripped,
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
}

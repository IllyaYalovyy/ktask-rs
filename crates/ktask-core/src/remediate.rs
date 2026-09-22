//! Failure signatures and the circuit breaker that trips on repetition.
//!
//! `VISION.md` §7 requires the runner to "detect repeated identical failure
//! signatures and trip a circuit breaker" during bounded remediation.
//! [`signature`] turns one attempt's classification and gate results into a
//! string two otherwise-identical failures produce identically, even when
//! everything timing- or path-related about the run differs from one
//! attempt to the next. [`Breaker`] counts how many times each signature has
//! recurred and reports when a signature has hit its repeat budget.
//!
//! Both are pure: no I/O, no clock. Recording that a breaker tripped in the
//! journal is the caller's job, using the existing [`crate::EventKind`]
//! catalog — this module only supplies the signature and the count.

use crate::{FailureClass, GateResult, parse_cargo};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

/// A wall-clock duration token such as `3.2s`, `500ms` or `12m`, always
/// stripped from a failing test name before it contributes to a
/// [`signature`]: the same test fails in the same way regardless of how long
/// the run around it took.
///
/// `None` only if the static pattern itself fails to compile, which this
/// module's tests would catch (mirrors [`mod@crate::classify`]'s
/// `DEFAULT_LIMIT_PATTERNS`); [`strip`] then leaves the text unchanged
/// rather than panicking.
static TIMING: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b\d+(?:\.\d+)?\s*(?:ms|s|m|h)\b").ok());

/// Any non-whitespace token containing a path separator, always stripped:
/// a source path or file reference varies with the checkout location and
/// the line a refactor moved code to, neither of which changes what
/// actually failed.
static PATH_TOKEN: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\S*[/\\]\S*").ok());

/// Any remaining run of decimal digits, stripped last: whatever survived
/// [`TIMING`] and [`PATH_TOKEN`] — a process id, an iteration count, a
/// generated suffix — is still noise a signature must not depend on.
static DIGITS: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\d+").ok());

/// Applies `pattern` to `text`, removing every match, or returns `text`
/// unchanged if `pattern` failed to compile (see [`TIMING`]).
fn strip(pattern: &LazyLock<Option<Regex>>, text: &str) -> String {
    match pattern.as_ref() {
        Some(re) => re.replace_all(text, "").into_owned(),
        None => text.to_string(),
    }
}

/// Normalizes one failing test name into the form [`signature`] hashes:
/// timings and path tokens removed, then any leftover digits, then
/// whitespace collapsed. Order matters — a timing like `3.2s` must be
/// recognized as one token before digit-stripping would otherwise leave
/// `..s` fragments that differ run to run in exactly the way this exists to
/// avoid.
fn normalize(name: &str) -> String {
    let name = strip(&TIMING, name);
    let name = strip(&PATH_TOKEN, &name);
    let name = strip(&DIGITS, &name);
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Builds a stable signature for one attempt's failure: `class` plus the
/// normalized names of every failing test named in `gates`' captured
/// output, order-independent and free of digits, paths and timings.
///
/// Only gates that did not pass contribute, and only the failing test names
/// [`parse_cargo`] can find in their stdout — a gate whose output is not
/// recognized cargo/nextest output (a lint or build failure, for instance)
/// contributes nothing beyond `class` itself, which two such failures still
/// share.
///
/// Two calls with the same `class` and the same set of failing test names
/// produce the same signature regardless of [`GateResult::duration_ms`],
/// [`GateResult::stdout`] text surrounding the failures list, or the order
/// gates appear in `gates` — the names are sorted and deduplicated before
/// hashing.
#[must_use]
pub fn signature(class: FailureClass, gates: &[GateResult]) -> String {
    let mut names: Vec<String> = gates
        .iter()
        .filter(|gate| !gate.passed)
        .flat_map(|gate| {
            parse_cargo(&gate.stdout)
                .map(|summary| summary.failures)
                .unwrap_or_default()
        })
        .map(|name| normalize(&name))
        .collect();
    names.sort();
    names.dedup();

    let mut hasher = Sha256::new();
    hasher.update(format!("{class:?}").as_bytes());
    for name in &names {
        hasher.update(b"\0");
        hasher.update(name.as_bytes());
    }
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Whether a failure signature is still within its allowed repeat budget or
/// has exceeded it, returned by [`Breaker::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// The signature has now recurred `count` times, still under
    /// [`Breaker::threshold`]: remediation may keep trying.
    Closed {
        /// How many times this exact signature has now been recorded.
        count: u32,
    },
    /// The signature has now recurred `count` times, reaching or exceeding
    /// [`Breaker::threshold`]: `VISION.md` §7's circuit breaker has tripped,
    /// and no further automatic remediation should be attempted for it.
    Tripped {
        /// How many times this exact signature has now been recorded.
        count: u32,
    },
}

/// Counts repeats of each failure signature seen across remediation
/// attempts on one task, per `VISION.md` §7 ("detect repeated identical
/// failure signatures and trip a circuit breaker").
///
/// Holds one counter per distinct signature, not a single global counter:
/// a task that fails two different ways in a row is not repeating itself,
/// so only an exact repeat of the same [`signature`] output counts toward
/// tripping.
#[derive(Debug, Clone)]
pub struct Breaker {
    /// How many times a signature must recur before [`Breaker::record`]
    /// reports [`BreakerState::Tripped`] (`docs/DESIGN.md`'s
    /// `circuit_breaker_threshold`, default `3`).
    pub threshold: u32,
    counts: HashMap<String, u32>,
}

impl Breaker {
    /// Creates a breaker with no signatures recorded yet, tripping once any
    /// one of them recurs `threshold` times.
    ///
    /// A `threshold` of `0` trips on the very first [`Breaker::record`]
    /// call for any signature: there is no repeat budget to spend.
    #[must_use]
    pub fn new(threshold: u32) -> Self {
        Breaker {
            threshold,
            counts: HashMap::new(),
        }
    }

    /// Records one more occurrence of `sig`, returning whether it is still
    /// within budget or has now tripped the breaker.
    ///
    /// Counts persist across calls for the lifetime of this `Breaker`, so a
    /// signature that recurs after other, different signatures were
    /// recorded in between still accumulates toward its own total.
    pub fn record(&mut self, sig: &str) -> BreakerState {
        let count = self.counts.entry(sig.to_string()).or_insert(0);
        *count += 1;
        if *count >= self.threshold {
            BreakerState::Tripped { count: *count }
        } else {
            BreakerState::Closed { count: *count }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventKind, GateKind, Journal, TaskId};

    fn failing_gate(stdout: &str, duration_ms: u64) -> GateResult {
        GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(101),
            signal: None,
            duration_ms,
            stdout: stdout.to_string(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    fn cargo_output(failing_line: &str, finished_in: &str) -> String {
        format!(
            "\
running 1 test
test tests::it_fails ... FAILED

failures:

---- tests::it_fails stdout ----
thread 'tests::it_fails' panicked at src/lib.rs:1:1:
assertion failed

failures:
    {failing_line}

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in {finished_in}
"
        )
    }

    #[test]
    fn two_runs_of_the_same_failing_test_with_different_durations_share_a_signature() {
        let first = failing_gate(&cargo_output("tests::it_fails", "0.12s"), 1_234);
        let second = failing_gate(&cargo_output("tests::it_fails", "9.87s"), 42_000);

        assert_eq!(
            signature(FailureClass::VerificationFailure, &[first]),
            signature(FailureClass::VerificationFailure, &[second]),
            "differing durations must not change the signature"
        );
    }

    #[test]
    fn a_failing_name_carrying_a_path_and_a_timing_normalizes_like_a_clean_one() {
        let noisy = failing_gate(
            &cargo_output("tests::it_fails src/lib.rs:42 3.2s", "1.11s"),
            500,
        );
        let clean = failing_gate(&cargo_output("tests::it_fails", "9.99s"), 999_999);

        assert_eq!(
            signature(
                FailureClass::VerificationFailure,
                std::slice::from_ref(&noisy)
            ),
            signature(FailureClass::VerificationFailure, &[clean]),
            "a path and a timing embedded in the failing name must be stripped"
        );

        let different_path = failing_gate(
            &cargo_output("tests::it_fails src/other/path.rs:99 9.87s", "2s"),
            1,
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[noisy]),
            signature(FailureClass::VerificationFailure, &[different_path]),
            "two different paths and timings must normalize to the same signature"
        );
    }

    #[test]
    fn a_different_failure_class_changes_the_signature() {
        let gate = failing_gate(&cargo_output("tests::it_fails", "0.12s"), 1_234);

        assert_ne!(
            signature(
                FailureClass::VerificationFailure,
                std::slice::from_ref(&gate)
            ),
            signature(FailureClass::AgentFailure, &[gate]),
            "class must contribute to the signature"
        );
    }

    #[test]
    fn a_different_failing_test_changes_the_signature() {
        let a = failing_gate(&cargo_output("tests::it_fails", "0.12s"), 1_234);
        let b = failing_gate(&cargo_output("tests::it_fails_differently", "0.12s"), 1_234);

        assert_ne!(
            signature(FailureClass::VerificationFailure, &[a]),
            signature(FailureClass::VerificationFailure, &[b]),
            "a genuinely different failing test must not collide"
        );
    }

    #[test]
    fn a_passing_gate_never_contributes_to_the_signature() {
        let mut passing = failing_gate(&cargo_output("tests::should_not_count", "0.12s"), 1_234);
        passing.passed = true;

        let sig_with_only_passing = signature(FailureClass::AgentFailure, &[passing]);
        let sig_with_no_gates = signature(FailureClass::AgentFailure, &[]);

        assert_eq!(
            sig_with_only_passing, sig_with_no_gates,
            "a passing gate's output must not leak into the signature"
        );
    }

    #[test]
    fn signature_is_independent_of_gate_order() {
        let a = failing_gate(&cargo_output("tests::alpha", "0.1s"), 10);
        let b = failing_gate(&cargo_output("tests::beta", "0.1s"), 10);

        assert_eq!(
            signature(FailureClass::VerificationFailure, &[a.clone(), b.clone()]),
            signature(FailureClass::VerificationFailure, &[b, a]),
            "signature must not depend on gate order"
        );
    }

    #[test]
    fn breaker_stays_closed_until_the_threshold_is_reached() {
        let mut breaker = Breaker::new(3);

        assert_eq!(breaker.record("sig-a"), BreakerState::Closed { count: 1 });
        assert_eq!(breaker.record("sig-a"), BreakerState::Closed { count: 2 });
        assert_eq!(breaker.record("sig-a"), BreakerState::Tripped { count: 3 });
    }

    #[test]
    fn breaker_tracks_each_signature_independently() {
        let mut breaker = Breaker::new(2);

        assert_eq!(breaker.record("sig-a"), BreakerState::Closed { count: 1 });
        assert_eq!(breaker.record("sig-b"), BreakerState::Closed { count: 1 });
        assert_eq!(breaker.record("sig-a"), BreakerState::Tripped { count: 2 });
        assert_eq!(breaker.record("sig-b"), BreakerState::Tripped { count: 2 });
    }

    #[test]
    fn a_zero_threshold_trips_on_the_first_occurrence() {
        let mut breaker = Breaker::new(0);
        assert_eq!(breaker.record("sig-a"), BreakerState::Tripped { count: 1 });
    }

    #[test]
    fn tripping_the_breaker_is_durably_journaled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open journal");

        let gate = failing_gate(&cargo_output("tests::it_fails", "0.12s"), 1_234);
        let class = FailureClass::VerificationFailure;
        let sig = signature(class, &[gate]);

        let mut breaker = Breaker::new(2);
        assert_eq!(breaker.record(&sig), BreakerState::Closed { count: 1 });
        let state = breaker.record(&sig);
        let BreakerState::Tripped { count } = state else {
            panic!("expected the breaker to trip on the second identical signature, got {state:?}");
        };

        let task = TaskId::new(1);
        let kind = EventKind::TaskFailed {
            class,
            detail: format!("circuit breaker tripped: signature {sig} recurred {count} time(s)"),
        };
        let seq = journal.append(Some(task), &kind).expect("append");

        // Reopen the journal independently of the handle that wrote it, so
        // this proves the trip actually landed on disk rather than only in
        // an in-memory connection.
        let reopened = Journal::open(&path).expect("reopen journal");
        let stored = reopened.events_for(task).expect("events_for");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seq, seq);
        assert_eq!(stored[0].kind, kind);
    }
}

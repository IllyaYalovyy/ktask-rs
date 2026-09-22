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

use crate::{AttemptRecord, Error, FailureClass, GateResult, Result, Task, parse_cargo, redact};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

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

/// How much of one failing gate's captured output [`bundle`] keeps before
/// its overall `budget_bytes` is applied — bounds a single huge transcript
/// independent of how tight the caller's budget is.
const GATE_TAIL_BYTES: usize = 4_096;

/// Returns the longest suffix of `text` that is at most `max_bytes` long and
/// still valid UTF-8 (never splits a multi-byte character).
fn tail_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Returns the longest prefix of `text` that is at most `max_bytes` long and
/// still valid UTF-8.
fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Formats the part of a [`bundle`] that always matters most: the task's
/// identity, its failure classification, the tail of every failing gate's
/// captured output, and the diff summary.
fn format_essential(
    task: &Task,
    class: FailureClass,
    gates: &[GateResult],
    diff_summary: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Task {}: {}", task.id, task.title());
    let _ = writeln!(out, "Classification: {class:?}");

    let failing: Vec<&GateResult> = gates.iter().filter(|gate| !gate.passed).collect();
    out.push_str("\nFailing gates:\n");
    if failing.is_empty() {
        out.push_str("(none)\n");
    }
    for gate in failing {
        let _ = writeln!(
            out,
            "- {:?} (exit_code={:?}, signal={:?}, timed_out={})",
            gate.kind, gate.exit_code, gate.signal, gate.timed_out
        );
        let combined = format!("{}{}", gate.stdout, gate.stderr);
        let text = tail_bytes(&combined, GATE_TAIL_BYTES).trim();
        if !text.is_empty() {
            out.push_str(text);
            out.push('\n');
        }
    }

    out.push_str("\nDiff summary:\n");
    out.push_str(diff_summary.trim());
    out.push('\n');
    out
}

/// Formats one prior attempt's outcome for [`bundle`]'s history section:
/// its id, why it ended, and each gate it ran with a pass/fail verdict —
/// never the gates' raw output, which belongs to the current failure only.
fn format_prior_attempt(record: &AttemptRecord) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Prior attempt {}: exit_reason={}",
        record.id, record.exit_reason
    );
    for gate in &record.gates {
        let _ = writeln!(
            out,
            "  - {:?}: {}",
            gate.kind,
            if gate.passed { "passed" } else { "failed" }
        );
    }
    out
}

/// Joins `essential` with every entry of `history`, in order, each separated
/// by a blank line.
fn join_bundle(essential: &str, history: &[String]) -> String {
    let mut out = essential.to_string();
    for entry in history {
        out.push('\n');
        out.push_str(entry);
    }
    out
}

/// Assembles the compact, deterministic context a fresh remediation session
/// starts from (`VISION.md` §7: "a compact failure bundle (classification,
/// gate output, diff summary, prior attempt evidence)"). Every remediation
/// attempt starts a fresh session rather than resuming one, so this bundle —
/// not conversation history — is the only context a retry gets.
///
/// The result always holds, in order: the task's identity and failure
/// [`FailureClass`], the tail of every currently failing gate's captured
/// output, the diff summary, and as much of `prior`'s attempt history as
/// fits, oldest first.
///
/// When the assembled text would exceed `budget_bytes`, whole prior-attempt
/// entries are dropped starting with the oldest (`prior[0]`, then
/// `prior[1]`, ...) until it fits. If even the classification, gate tails
/// and diff summary alone exceed the budget, the result is cut to
/// `budget_bytes` bytes at the nearest character boundary — the bundle never
/// exceeds its budget, whatever it costs to enforce that.
///
/// Every piece is passed through [`redact()`] before it is measured or
/// truncated, so a secret is never split by truncation into a still-partly-
/// readable fragment.
///
/// This function reads no clock, no environment and no random source, and
/// preserves the order `gates` and `prior` were given in without sorting —
/// two calls with identical arguments always produce a byte-identical
/// result.
#[must_use]
pub fn bundle(
    task: &Task,
    class: FailureClass,
    gates: &[GateResult],
    diff_summary: &str,
    prior: &[AttemptRecord],
    budget_bytes: usize,
) -> String {
    let essential = redact(&format_essential(task, class, gates, diff_summary), &[]);
    let history: Vec<String> = prior
        .iter()
        .map(|record| redact(&format_prior_attempt(record), &[]))
        .collect();

    for drop in 0..=history.len() {
        let remaining = history.get(drop..).unwrap_or_default();
        let candidate = join_bundle(&essential, remaining);
        if candidate.len() <= budget_bytes {
            return candidate;
        }
    }

    truncate_bytes(&essential, budget_bytes)
}

/// The hard limits `VISION.md` §7 requires remediation to respect: "Bound
/// remediation by attempts, elapsed time, and token budget." Checked by
/// [`should_continue`] on every remediation attempt, before a fresh provider
/// session is launched.
///
/// `max_tokens` is `None` when no token budget applies (a provider that does
/// not report usage, or a caller that has not configured one) — attempts and
/// elapsed time still bound the loop in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// The most remediation attempts to make for one task, including the
    /// one that just failed.
    pub max_attempts: u32,
    /// The longest wall-clock time remediation may spend on one task, timed
    /// from the first attempt.
    pub max_elapsed: Duration,
    /// The most tokens remediation may spend on one task, summed across all
    /// its attempts, or `None` if unbounded.
    pub max_tokens: Option<u64>,
}

/// Names which of [`Bounds`]' three limits [`should_continue`] found
/// exceeded, and the values that tripped it — carried by
/// [`Decision::Stop`] so a caller can journal and report exactly why
/// remediation stopped rather than just that it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `attempts` has reached or passed [`Bounds::max_attempts`].
    Attempts {
        /// The attempt count that tripped the bound.
        attempts: u32,
        /// The [`Bounds::max_attempts`] value it reached or passed.
        max_attempts: u32,
    },
    /// `elapsed` has reached or passed [`Bounds::max_elapsed`].
    Elapsed {
        /// The elapsed time that tripped the bound.
        elapsed: Duration,
        /// The [`Bounds::max_elapsed`] value it reached or passed.
        max_elapsed: Duration,
    },
    /// `tokens` has reached or passed [`Bounds::max_tokens`] (only possible
    /// when that bound is `Some`).
    Tokens {
        /// The token count that tripped the bound.
        tokens: u64,
        /// The [`Bounds::max_tokens`] value it reached or passed.
        max_tokens: u64,
    },
}

/// Whether another remediation attempt may be launched, returned by
/// [`should_continue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Every bound is still within budget: a fresh remediation session may
    /// be launched.
    Continue,
    /// At least one bound in [`Bounds`] has been reached or exceeded,
    /// naming which one via [`StopReason`]. No further automatic
    /// remediation should be attempted.
    Stop(StopReason),
}

/// Checks `attempts`, `elapsed` and `tokens` so far against `bounds`, per
/// `VISION.md` §7: "Bound remediation by attempts, elapsed time, and token
/// budget."
///
/// Bounds are checked in a fixed order — attempts, then elapsed time, then
/// tokens — and the first one reached or exceeded is reported; the caller
/// does not need to know that order to get a correct answer, since only one
/// [`StopReason`] is ever returned regardless of how many bounds are
/// simultaneously exceeded.
///
/// A bound is tripped once its counter reaches or exceeds the configured
/// limit, not only once it strictly exceeds it — an `attempts` count equal
/// to `max_attempts` has already spent the whole budget, with nothing left
/// for one more try.
#[must_use]
pub fn should_continue(bounds: &Bounds, attempts: u32, elapsed: Duration, tokens: u64) -> Decision {
    if attempts >= bounds.max_attempts {
        return Decision::Stop(StopReason::Attempts {
            attempts,
            max_attempts: bounds.max_attempts,
        });
    }
    if elapsed >= bounds.max_elapsed {
        return Decision::Stop(StopReason::Elapsed {
            elapsed,
            max_elapsed: bounds.max_elapsed,
        });
    }
    if let Some(max_tokens) = bounds.max_tokens
        && tokens >= max_tokens
    {
        return Decision::Stop(StopReason::Tokens { tokens, max_tokens });
    }
    Decision::Continue
}

/// Repository-root-relative directories no attempt's diff may touch:
/// `VISION.md` §2 ("no self-modification of policy") and §3 invariant 5
/// ("self-healing cannot weaken checks, change policy, ... or conceal
/// failures"). Matched component-wise against the start of each path, so a
/// sibling like `scripts-vendor/` does not collide with `scripts/`.
const PROTECTED_DIRS: [&str; 2] = ["scripts", ".ktask"];

/// Gate-configuration file names no attempt's diff may touch, wherever in
/// the tree they appear: `VISION.md` §7's `policy_failure` class names
/// "attempted gate bypass" explicitly, and these three files are exactly
/// what `scripts/quality.sh`'s `fmt`, `clippy` and `deny` gates are
/// configured by.
const PROTECTED_FILES: [&str; 3] = ["deny.toml", "clippy.toml", "rustfmt.toml"];

/// Returns whether `path` is one this project's gates are configured by, or
/// lives under a directory the runner itself relies on.
fn is_protected(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|name| name.to_str())
        && PROTECTED_FILES.contains(&name)
    {
        return true;
    }
    PROTECTED_DIRS
        .iter()
        .any(|dir| path.starts_with(Path::new(dir)))
}

/// Fails if any path in `diff_paths` touches gate configuration
/// (`deny.toml`, `clippy.toml`, `rustfmt.toml`), `scripts/`, or `.ktask/` —
/// the files and directories that define what a task is judged by
/// (`VISION.md` §2: "no self-modification of policy"; §3 invariant 5:
/// "self-healing cannot weaken checks, change policy, ... or conceal
/// failures").
///
/// Called against the real diff for **every** attempt, not only a
/// remediation: an ordinary first attempt that edits `clippy.toml` is the
/// same violation as a remediation attempt doing the same thing, and both
/// are rejected identically — nothing here distinguishes the two.
///
/// `diff_paths` is expected in the same repository-root-relative form
/// [`crate::status_porcelain`] and [`crate::require_clean`] use (as
/// produced by `git diff --name-only` and friends); an absolute path never
/// matches a protected directory prefix.
///
/// # Errors
///
/// Returns [`Error::Policy`] naming every offending path, not merely the
/// first, when at least one path in `diff_paths` is protected.
pub fn check_no_policy_edit(diff_paths: &[PathBuf]) -> Result<()> {
    let offending: Vec<PathBuf> = diff_paths
        .iter()
        .filter(|path| is_protected(path))
        .cloned()
        .collect();

    if offending.is_empty() {
        return Ok(());
    }

    Err(Error::Policy {
        detail: "diff touches protected gate configuration".to_string(),
        paths: offending,
    })
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

    mod bundle_tests {
        use super::*;
        use crate::{AttemptId, Task, TaskStatus, Usage, UsageSource};
        use time::macros::datetime;

        fn sample_task() -> Task {
            Task {
                id: TaskId::new(1),
                status: TaskStatus::Pending,
                body: "## Do the thing\n".to_string(),
                outcome: "it happens".to_string(),
                done_when: "it happened".to_string(),
                verify: "cargo test".to_string(),
                refs: "VISION.md".to_string(),
            }
        }

        fn passing_gate() -> GateResult {
            GateResult {
                kind: GateKind::Lint,
                passed: true,
                exit_code: Some(0),
                signal: None,
                duration_ms: 10,
                stdout: "clean".to_string(),
                stderr: String::new(),
                timed_out: false,
            }
        }

        fn failing_gate(stdout: &str) -> GateResult {
            GateResult {
                kind: GateKind::Verify,
                passed: false,
                exit_code: Some(101),
                signal: None,
                duration_ms: 500,
                stdout: stdout.to_string(),
                stderr: String::new(),
                timed_out: false,
            }
        }

        fn prior_attempt(id: u32, exit_reason: &str) -> AttemptRecord {
            AttemptRecord {
                id: AttemptId::new(id),
                task: TaskId::new(1),
                started: datetime!(2024-01-15 10:00:00 UTC),
                ended: Some(datetime!(2024-01-15 10:05:00 UTC)),
                model_configured: Some("claude-opus-4".to_string()),
                model_reported: None,
                session_id: None,
                exit_reason: exit_reason.to_string(),
                gates: vec![failing_gate("boom")],
                usage: Some(Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    cached_tokens: None,
                    cost_usd: None,
                    source: UsageSource::Provider,
                }),
                base_sha: "base".to_string(),
                candidate_sha: None,
            }
        }

        #[test]
        fn identical_inputs_produce_a_byte_identical_bundle() {
            let task = sample_task();
            let gates = vec![failing_gate("assertion failed at line 12")];
            let prior = vec![prior_attempt(1, "verification_failure")];

            let first = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "3 files changed",
                &prior,
                8_192,
            );
            let second = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "3 files changed",
                &prior,
                8_192,
            );

            assert_eq!(first, second, "identical inputs must yield identical bytes");
        }

        #[test]
        fn the_bundle_includes_classification_gate_tail_diff_summary_and_prior_outcomes() {
            let task = sample_task();
            let gates = vec![passing_gate(), failing_gate("assertion failed at line 12")];
            let prior = vec![prior_attempt(1, "provider_transient")];

            let text = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "diff: +10/-2 across 2 files",
                &prior,
                8_192,
            );

            assert!(text.contains("VerificationFailure"));
            assert!(text.contains("assertion failed at line 12"));
            assert!(text.contains("diff: +10/-2 across 2 files"));
            assert!(text.contains("provider_transient"));
        }

        #[test]
        fn a_passing_gates_output_never_appears() {
            let task = sample_task();
            let gates = vec![passing_gate()];

            let text = bundle(
                &task,
                FailureClass::AgentFailure,
                &gates,
                "no diff",
                &[],
                8_192,
            );

            assert!(!text.contains("clean"), "a passing gate's stdout leaked in");
        }

        #[test]
        fn the_bundle_never_exceeds_its_budget_even_with_a_huge_gate_and_deep_history() {
            let task = sample_task();
            let gates = vec![failing_gate(&"x".repeat(50_000))];
            let prior: Vec<AttemptRecord> = (1..=20)
                .map(|id| prior_attempt(id, "verification_failure"))
                .collect();

            for budget in [0, 1, 50, 500, 4_000, 100_000] {
                let text = bundle(
                    &task,
                    FailureClass::VerificationFailure,
                    &gates,
                    "a modest diff",
                    &prior,
                    budget,
                );
                assert!(
                    text.len() <= budget,
                    "budget {budget} exceeded: got {} bytes",
                    text.len()
                );
            }
        }

        #[test]
        fn truncation_drops_the_oldest_prior_attempts_first() {
            let task = sample_task();
            let gates = vec![failing_gate("short failure")];
            let prior = vec![
                prior_attempt(1, "oldest-attempt-marker"),
                prior_attempt(2, "newest-attempt-marker"),
            ];

            // A budget that comfortably fits the essential section and one
            // history entry, but not both prior attempts.
            let essential_only = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "a modest diff",
                &[],
                8_192,
            );
            let budget = essential_only.len() + 80;

            let text = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "a modest diff",
                &prior,
                budget,
            );

            assert!(
                text.contains("newest-attempt-marker"),
                "the most recent attempt must survive truncation: {text}"
            );
            assert!(
                !text.contains("oldest-attempt-marker"),
                "the oldest attempt must be dropped first: {text}"
            );
        }

        #[test]
        fn a_secret_in_gate_output_does_not_survive() {
            let task = sample_task();
            let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
            let gates = vec![failing_gate(&format!("leaked credential: {secret}"))];

            let text = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "no diff",
                &[],
                8_192,
            );

            assert!(!text.contains(secret));
            assert!(text.contains("[redacted]"));
        }

        #[test]
        fn a_secret_in_prior_attempt_data_does_not_survive() {
            let task = sample_task();
            let gates = vec![failing_gate("clean failure")];
            let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789AB";
            let prior = vec![prior_attempt(1, &format!("crashed on token {secret}"))];

            let text = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "no diff",
                &prior,
                8_192,
            );

            assert!(!text.contains(secret));
        }

        #[test]
        fn a_secret_never_survives_even_when_truncation_cuts_through_it() {
            let task = sample_task();
            // A generous budget for the header, but far too small to fit
            // this gate's full output, so hard truncation must engage after
            // redaction has already removed the secret.
            let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
            let padding = "y".repeat(200);
            let gates = vec![failing_gate(&format!(
                "{padding} leaked credential: {secret} {padding}"
            ))];

            let text = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "no diff",
                &[],
                120,
            );

            assert!(text.len() <= 120);
            assert!(!text.contains(secret));
            assert!(!text.contains(&secret[..10]));
        }

        #[test]
        fn only_the_tail_of_a_huge_gate_output_is_kept() {
            let task = sample_task();
            let huge = format!("{}END-OF-OUTPUT", "a".repeat(20_000));
            let gates = vec![failing_gate(&huge)];

            let text = bundle(
                &task,
                FailureClass::VerificationFailure,
                &gates,
                "no diff",
                &[],
                1_000_000,
            );

            assert!(text.contains("END-OF-OUTPUT"));
            assert!(
                text.len() < huge.len(),
                "the gate's full 20000-byte output must not all be kept"
            );
        }
    }

    mod bounds_tests {
        use super::*;

        fn generous_bounds() -> Bounds {
            Bounds {
                max_attempts: 100,
                max_elapsed: Duration::from_secs(3_600),
                max_tokens: Some(1_000_000),
            }
        }

        #[test]
        fn within_every_bound_continues() {
            let bounds = generous_bounds();
            assert_eq!(
                should_continue(&bounds, 1, Duration::from_secs(1), 10),
                Decision::Continue
            );
        }

        #[test]
        fn reaching_max_attempts_alone_stops_and_names_attempts() {
            let bounds = generous_bounds();
            let decision = should_continue(&bounds, bounds.max_attempts, Duration::ZERO, 0);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Attempts {
                    attempts: bounds.max_attempts,
                    max_attempts: bounds.max_attempts,
                })
            );
        }

        #[test]
        fn exceeding_max_attempts_alone_stops_and_names_attempts() {
            let bounds = generous_bounds();
            let decision = should_continue(&bounds, bounds.max_attempts + 5, Duration::ZERO, 0);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Attempts {
                    attempts: bounds.max_attempts + 5,
                    max_attempts: bounds.max_attempts,
                })
            );
        }

        #[test]
        fn reaching_max_elapsed_alone_stops_and_names_elapsed() {
            let bounds = generous_bounds();
            let decision = should_continue(&bounds, 0, bounds.max_elapsed, 0);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Elapsed {
                    elapsed: bounds.max_elapsed,
                    max_elapsed: bounds.max_elapsed,
                })
            );
        }

        #[test]
        fn exceeding_max_elapsed_alone_stops_and_names_elapsed() {
            let bounds = generous_bounds();
            let over = bounds.max_elapsed + Duration::from_secs(1);
            let decision = should_continue(&bounds, 0, over, 0);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Elapsed {
                    elapsed: over,
                    max_elapsed: bounds.max_elapsed,
                })
            );
        }

        #[test]
        fn reaching_max_tokens_alone_stops_and_names_tokens() {
            let bounds = generous_bounds();
            let max_tokens = bounds.max_tokens.expect("bound configured");
            let decision = should_continue(&bounds, 0, Duration::ZERO, max_tokens);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Tokens {
                    tokens: max_tokens,
                    max_tokens,
                })
            );
        }

        #[test]
        fn exceeding_max_tokens_alone_stops_and_names_tokens() {
            let bounds = generous_bounds();
            let max_tokens = bounds.max_tokens.expect("bound configured");
            let decision = should_continue(&bounds, 0, Duration::ZERO, max_tokens + 1);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Tokens {
                    tokens: max_tokens + 1,
                    max_tokens,
                })
            );
        }

        #[test]
        fn no_token_bound_never_stops_on_tokens_however_high() {
            let bounds = Bounds {
                max_attempts: 100,
                max_elapsed: Duration::from_secs(3_600),
                max_tokens: None,
            };

            assert_eq!(
                should_continue(&bounds, 0, Duration::ZERO, u64::MAX),
                Decision::Continue,
                "an unset token bound must never trip, no matter how many tokens were spent"
            );
        }

        #[test]
        fn attempts_is_checked_before_elapsed_or_tokens_when_several_bounds_trip_together() {
            let bounds = Bounds {
                max_attempts: 3,
                max_elapsed: Duration::from_secs(10),
                max_tokens: Some(100),
            };

            let decision = should_continue(&bounds, 3, Duration::from_secs(999), 999);

            assert_eq!(
                decision,
                Decision::Stop(StopReason::Attempts {
                    attempts: 3,
                    max_attempts: 3,
                }),
                "with several bounds tripped at once, attempts must be the reported reason"
            );
        }

        #[test]
        fn zero_max_attempts_stops_before_the_very_first_attempt() {
            let bounds = Bounds {
                max_attempts: 0,
                max_elapsed: Duration::from_secs(3_600),
                max_tokens: None,
            };

            assert_eq!(
                should_continue(&bounds, 0, Duration::ZERO, 0),
                Decision::Stop(StopReason::Attempts {
                    attempts: 0,
                    max_attempts: 0,
                })
            );
        }
    }

    mod policy_tests {
        use super::*;

        fn paths(names: &[&str]) -> Vec<PathBuf> {
            names.iter().map(PathBuf::from).collect()
        }

        #[test]
        fn ordinary_source_changes_pass() {
            let diff = paths(&[
                "crates/ktask-core/src/remediate.rs",
                "crates/ktask-cli/src/main.rs",
                "Cargo.toml",
                "Cargo.lock",
                "docs/adr/0001-example.md",
            ]);
            assert!(check_no_policy_edit(&diff).is_ok());
        }

        #[test]
        fn a_first_attempt_touching_scripts_is_rejected() {
            // "First attempt" here means: nothing about this call marks it
            // as remediation. The function takes no such flag, which is
            // exactly the point — an ordinary attempt is checked the same
            // way a remediation attempt is.
            let diff = paths(&["scripts/quality.sh"]);
            let err = check_no_policy_edit(&diff).expect_err("must reject");
            assert!(
                matches!(&err, Error::Policy { paths, .. } if paths == &[PathBuf::from("scripts/quality.sh")])
            );
        }

        #[test]
        fn a_remediation_diff_touching_scripts_is_rejected_identically() {
            let diff = paths(&["scripts/quality.sh"]);
            let first_attempt = check_no_policy_edit(&diff);
            let remediation = check_no_policy_edit(&diff);
            assert_eq!(
                first_attempt.is_err(),
                remediation.is_err(),
                "the check must not distinguish remediation from a first attempt"
            );
            assert!(remediation.is_err());
        }

        #[test]
        fn a_diff_touching_dot_ktask_is_rejected() {
            let diff = paths(&[".ktask/config.toml"]);
            let err = check_no_policy_edit(&diff).expect_err("must reject");
            assert!(matches!(err, Error::Policy { .. }));
        }

        #[test]
        fn a_nested_file_under_a_protected_directory_is_rejected() {
            let diff = paths(&[".ktask/queue/report-1.md"]);
            let err = check_no_policy_edit(&diff).expect_err("must reject");
            assert!(matches!(err, Error::Policy { .. }));
        }

        #[test]
        fn each_named_gate_configuration_file_is_rejected() {
            for name in ["deny.toml", "clippy.toml", "rustfmt.toml"] {
                let diff = paths(&[name]);
                assert!(
                    check_no_policy_edit(&diff).is_err(),
                    "{name} must be rejected"
                );
            }
        }

        #[test]
        fn a_gate_configuration_file_nested_under_another_directory_is_still_rejected() {
            // A task cannot dodge the check by relocating the file the
            // gate actually reads elsewhere in the tree.
            let diff = paths(&["some/nested/dir/clippy.toml"]);
            assert!(check_no_policy_edit(&diff).is_err());
        }

        #[test]
        fn a_similarly_named_sibling_directory_is_not_protected() {
            // `scripts-vendor/` and `.ktasks/` share a prefix with a
            // protected directory but are not the directory itself.
            let diff = paths(&["scripts-vendor/tool.sh", ".ktasks/notes.md"]);
            assert!(check_no_policy_edit(&diff).is_ok());
        }

        #[test]
        fn every_offending_path_is_named_not_only_the_first() {
            let diff = paths(&[
                "src/lib.rs",
                "scripts/quality.sh",
                ".ktask/config.toml",
                "deny.toml",
            ]);
            let err = check_no_policy_edit(&diff).expect_err("must reject");
            let Error::Policy { paths, .. } = &err else {
                panic!("expected Error::Policy, got {err:?}");
            };
            assert_eq!(paths.len(), 3, "the one clean path must not be reported");
            assert!(paths.contains(&PathBuf::from("scripts/quality.sh")));
            assert!(paths.contains(&PathBuf::from(".ktask/config.toml")));
            assert!(paths.contains(&PathBuf::from("deny.toml")));
        }

        #[test]
        fn an_empty_diff_passes() {
            assert!(check_no_policy_edit(&[]).is_ok());
        }

        #[test]
        fn a_rejected_diff_is_durably_journaled_as_a_policy_failure() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("journal.db");
            let mut journal = Journal::open(&path).expect("open journal");

            let diff = paths(&["scripts/quality.sh"]);
            let err = check_no_policy_edit(&diff).expect_err("must reject");
            let Error::Policy { detail, paths } = &err else {
                panic!("expected Error::Policy, got {err:?}");
            };

            let task = TaskId::new(1);
            let kind = EventKind::VerifyFailed {
                attempt: crate::AttemptId::new(1),
                class: FailureClass::PolicyFailure,
                detail: format!("{detail} ({paths:?})"),
            };
            let seq = journal.append(Some(task), &kind).expect("append");

            // Reopen independently of the writing handle, so this proves
            // the rejection actually landed on disk rather than only in an
            // in-memory connection.
            let reopened = Journal::open(&path).expect("reopen journal");
            let stored = reopened.events_for(task).expect("events_for");
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].seq, seq);
            assert_eq!(stored[0].kind, kind);
            assert!(matches!(
                &stored[0].kind,
                EventKind::VerifyFailed {
                    class: FailureClass::PolicyFailure,
                    ..
                }
            ));
        }
    }
}

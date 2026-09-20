//! What a repeated failure looks like, and the breaker that stops counting it.
//!
//! VISION.md §7 asks for two things of self-healing that bound it: repeated
//! identical failure signatures are *detected*, and a circuit breaker then
//! trips. Both need one thing no other module produces: a name for a failure
//! that survives being seen twice. A gate result is the wrong shape for that —
//! it carries the seconds the command ran, the checkout it ran in and the line
//! number a panic landed on, and all three move between two runs of the very
//! same broken test. [`signature`] removes them and hashes what is left, so
//! "the same failure again" is a comparison of two short strings rather than a
//! judgement about a transcript.
//!
//! # What the signature is over
//!
//! The failure class, and the failing test names the refusing gates reported.
//! Names come from [`crate::parse_cargo`] — the same reading that gives the
//! TUI's failures screen its list — so the signature and the screen agree about
//! which tests refused. A gate that refused without naming any test (a build
//! that never compiled, a lint that never ran one) contributes its own kind
//! beside the first line of what it did say, which keeps two different
//! refusals apart instead of collapsing every nameless failure into one
//! signature that would trip the breaker on unrelated work.
//!
//! Names are then normalized: digits, paths and timings go, because those are
//! the three things a rerun changes while the failure stays. `case_1` and
//! `case_12` are one parametrised test failing; `/home/alice/wt/…` and
//! `/home/ci/agent-7/checkout/…` are one file in two checkouts; `took 3s` and
//! `took 4200ms` are one refusal that waited longer. The set of names is sorted
//! and deduplicated before it is hashed, so a run that reported its two
//! failures in the other order, or one test that two test binaries both
//! reported, is the failure it already was.
//!
//! The cost of stripping digits is that two tests whose names differ *only* by
//! a digit are one signature. That is accepted deliberately: a test named
//! `case_1` and one named `case_2` are one body of code with two inputs, and
//! the answer VISION.md §7 wants from a breaker is the same for both.
//!
//! # What a trip is worth
//!
//! [`Breaker`] counts a signature, not a run: the threshold in the project's
//! configuration is a count of *identical* failures, and a failure that
//! alternates with a different one is still the same failure returning. Once a
//! signature has reached the threshold the breaker is tripped and stays
//! tripped — a breaker that closes again because the next failure was
//! worded differently is a counter, not a breaker. [`trip_event`] is the
//! journal record a trip leaves behind, so the run that stopped says so in the
//! same append-only file every other decision is in.

//! The failure bundle VISION.md §7 asks every remediation session to be seeded
//! with is not here. `docs/DESIGN.md` files it beside the breaker because both
//! belong to `remediate.rs`, but a bundle is assembled from a diff and a prior
//! attempt's evidence, which is the runner's to hand over.

use regex::Regex;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use crate::{EventKind, FailureClass, GateResult, parse_cargo};

/// How many hexadecimal characters a signature carries.
///
/// Sixty-four bits is far past what comparing the handful of signatures one task
/// produces needs, and a signature is read by a human on the failures screen as
/// well as compared by a breaker — the project id is truncated for the same
/// reason (ADR-0003).
const SIGNATURE_HEX_CHARS: usize = 16;

/// One normalization step: a shape to remove and what a match of it becomes.
///
/// The shape is compiled the first time it is read, the way every other phrase
/// table in this crate is (ADR-0059), and a shape that does not compile is
/// skipped rather than panicking — a supervisor that panics loses the run. That
/// is only honest because `every_normalization_rule_compiles` insists the four
/// below do: a silently missing rule would make two runs of one failure look
/// like two failures, which spends a remediation budget on a test nobody
/// changed.
struct Rule {
    /// The shape as it is written in this file.
    source: &'static str,
    /// What a match of it becomes.
    replacement: &'static str,
    /// The same shape compiled, on first use.
    compiled: OnceLock<Option<Regex>>,
}

impl Rule {
    /// A shape that is not compiled yet.
    const fn new(source: &'static str, replacement: &'static str) -> Self {
        Self {
            source,
            replacement,
            compiled: OnceLock::new(),
        }
    }

    /// The compiled shape, or `None` when the shape is not a regular expression.
    fn regex(&self) -> Option<&Regex> {
        self.compiled
            .get_or_init(|| Regex::new(self.source).ok())
            .as_ref()
    }

    /// This shape applied to `text`, which is `text` itself when the shape
    /// never compiled.
    fn apply(&self, text: &str) -> String {
        match self.regex() {
            Some(pattern) => pattern.replace_all(text, self.replacement).into_owned(),
            None => text.to_owned(),
        }
    }
}

/// A number beside the unit that says how long something took.
///
/// The units are ordered longest first so `250ms` is a quarter of a second and
/// not `250m` with an `s` left over. The unit goes with the number because a
/// rule that removed only the digits would leave `took s` beside `took ms` and
/// call one refusal two.
static TIMINGS: Rule = Rule::new(
    concat!(
        r"(?i)\b\d+(?:[.,]\d+)?[ \t]*(milliseconds?|ms|microseconds?|us|nanoseconds?|ns|",
        r"weeks?|wks?|w|days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|sec|s)\b",
    ),
    "",
);

/// A directory, with the separator that closes it.
///
/// Only a segment that *ends* in a separator goes, so the file a message names
/// survives and the checkout it lives in does not: the same broken file in
/// `/home/alice/wt/` and in `/home/ci/agent-7/checkout/` is one failure, and
/// the directory a run happened to be in is not part of what failed.
static PATHS: Rule = Rule::new(r"[\w.\-]+[/\\]+", "");

/// Any run of digits, which is where line numbers, counters, byte counts and
/// parametrised case numbers all live.
static DIGITS: Rule = Rule::new(r"\d+", "");

/// Punctuation and whitespace run together, which is what stripping leaves
/// behind: `gate.rs:41:5:` becomes `gate.rs` and `a::b` becomes `a b`.
///
/// Underscores are deliberately absent. They are what joins the words of a
/// Rust test name, and removing them would smear every name into the same run
/// of letters.
static SEPARATORS: Rule = Rule::new(r"[\s:/\\.]+", " ");

/// The four rules, in the order they are applied.
///
/// Held by reference because a `Rule` compiles itself once and so is shared,
/// never copied.
static RULES: [&Rule; 4] = [&TIMINGS, &PATHS, &DIGITS, &SEPARATORS];

/// Name a failure so a later one can be recognized as it.
///
/// The class is hashed *with* the names rather than beside them: the class is
/// what decides the response (VISION.md §7), so one test that fails once as a
/// [`FailureClass::VerificationFailure`] and once as a
/// [`FailureClass::EnvironmentFailure`] is two failures, and counting them
/// together would spend a breaker on a machine that changed underneath the run.
///
/// `gates` is the whole set a run recorded, passing gates included: only the ones
/// that refused contribute, so one broken test keeps one signature whether or not
/// the lint gate happened to run first.
#[must_use]
pub fn signature(class: FailureClass, gates: &[GateResult]) -> String {
    let mut names = BTreeSet::new();
    for gate in gates.iter().filter(|gate| !gate.passed) {
        contribute(gate, &mut names);
    }
    // The class name is the spelling the journal writes the class with in its
    // own payload, which `classify`'s vocabulary test pins, so the two agree
    // without a second table of words to keep in step.
    let mut key = format!("{class:?}");
    for name in names {
        key.push('\n');
        key.push_str(&name);
    }
    hashed(&key)
}

/// Add one refusing gate's failure names to `names`.
///
/// A gate that named its failures contributes every one of them; a gate that
/// refused without naming any contributes its kind beside the first line it did
/// write, which is what keeps two unrelated nameless refusals apart.
fn contribute(gate: &GateResult, names: &mut BTreeSet<String>) {
    let reported = reported_names(gate);
    if reported.is_empty() {
        names.insert(silent_name(gate));
    } else {
        names.extend(reported);
    }
}

/// The tests a refusing gate reported by name, normalized.
///
/// The reading is [`parse_cargo`], the same one the failures screen lists, so
/// the signature and the screen cannot disagree about which tests refused.
/// Cargo writes both of its streams and a transcript arrives with the test names
/// in either, so the two are read as the one transcript they are.
fn reported_names(gate: &GateResult) -> Vec<String> {
    let transcript = format!("{}\n{}", gate.stdout, gate.stderr);
    parse_cargo(&transcript)
        .map(|summary| summary.failures)
        .unwrap_or_default()
        .iter()
        .map(|name| normalize(name))
        .filter(|name| !name.is_empty())
        .collect()
}

/// What a gate that named no test is known by: its kind, and the first line it
/// said anything on.
///
/// Stderr is read first because that is where a build or a lint writes its
/// verdict, and the first line is the first thing a repeated refusal repeats.
/// A gate that refused in silence is known by its kind alone.
fn silent_name(gate: &GateResult) -> String {
    let verdict = gate
        .stderr
        .lines()
        .chain(gate.stdout.lines())
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    normalize(&format!("{} {verdict}", gate.kind.as_str()))
}

/// Strip the three things a rerun changes while the failure stays: timings,
/// paths and digits, then collapse what the stripping left into single spaces.
fn normalize(text: &str) -> String {
    let mut text = text.to_owned();
    for rule in RULES {
        text = rule.apply(&text);
    }
    text.trim().to_owned()
}

/// What recording one more failure did to the breaker's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerState {
    /// This signature has now been recorded `seen` times, which is short of the
    /// threshold: another remediation is still paid for.
    Allowed {
        /// How many times this signature has been recorded.
        seen: u32,
    },
    /// The budget is spent. `signature` is the one that spent it and `seen` is
    /// how many times it arrived — the signature is repeated here because a
    /// breaker that is already tripped reports the trip it holds, not the
    /// failure that happened to arrive afterwards.
    Tripped {
        /// The signature whose repeats spent the budget.
        signature: String,
        /// How many records of it the threshold cost.
        seen: u32,
    },
}

/// Counts identical failure signatures until the configured threshold is spent.
///
/// A breaker belongs to one task's remediation, which is the caller's to
/// create: it is the object that answers "have we seen this before", and VISION.md
/// §7's bound is on identical failures, not on attempts — a failure that
/// alternates with a different one is still the same failure returning, and a
/// counter that forgot it whenever something else went wrong never trips at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breaker {
    /// How many records of one signature trip it.
    threshold: u32,
    /// How many times each signature has been recorded.
    seen: BTreeMap<String, u32>,
    /// The trip, once there has been one: a breaker does not close again.
    tripped: Option<(String, u32)>,
}

impl Breaker {
    /// A breaker that trips once one signature has been recorded `threshold`
    /// times — the project's `circuit_breaker_threshold`, which [`crate::Config`]
    /// defaults to three. A threshold of zero or one trips on the first record.
    #[must_use]
    pub const fn new(threshold: u32) -> Self {
        Self {
            threshold,
            seen: BTreeMap::new(),
            tripped: None,
        }
    }

    /// Record one more occurrence of `sig` and say what the budget looks like
    /// now.
    ///
    /// A trip is sticky. A run that tripped and then failed for a slightly
    /// different reason has not been fixed, and handing it another session
    /// because the next transcript was worded differently is how a bounded
    /// remediation becomes an unbounded one.
    pub fn record(&mut self, sig: &str) -> BreakerState {
        if let Some((signature, seen)) = &self.tripped {
            return BreakerState::Tripped {
                signature: signature.clone(),
                seen: *seen,
            };
        }
        let seen = self.seen.entry(sig.to_owned()).or_insert(0);
        *seen = seen.saturating_add(1);
        if *seen >= self.threshold {
            self.tripped = Some((sig.to_owned(), *seen));
            BreakerState::Tripped {
                signature: sig.to_owned(),
                seen: *seen,
            }
        } else {
            BreakerState::Allowed { seen: *seen }
        }
    }
}

/// The journal record a trip leaves behind.
///
/// A tripped breaker ends the task rather than launching another remediation,
/// and `docs/DESIGN.md` already has an entry for exactly that: `TaskFailed`,
/// whose `class` is the classified cause and whose `detail` says what ended it.
/// The catalog admits no entry without the `state::apply` arm that answers it,
/// and this one needs none — the machine already fails a task from `Running` and
/// from `Remediating` when a `TaskFailed` arrives (ADR-0022).
#[must_use]
pub fn trip_event(class: FailureClass, signature: &str, seen: u32) -> EventKind {
    EventKind::TaskFailed {
        class,
        detail: format!(
            "circuit breaker tripped: signature {signature} has now failed {seen} times, and \
             remediation is spent"
        ),
    }
}

/// The lowercase hexadecimal prefix of the SHA-256 of `text`.
fn hashed(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(SIGNATURE_HEX_CHARS / 2)
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::RULES;
    use super::{Breaker, BreakerState, signature, trip_event};
    use crate::{
        AttemptId, EventKind, FailureClass, GateKind, GateResult, Journal, Phase, TaskId,
        TaskState, apply, journal_path,
    };
    use proptest::prelude::*;
    use tempfile::{TempDir, tempdir};

    /// The signature's documented shape: this many lowercase hex characters.
    const HEX_CHARS: usize = 16;

    /// A scratch parent for a journal: `docs/DESIGN.md` Conventions forbids a
    /// test from writing inside the repository.
    fn scratch() -> TempDir {
        tempdir().expect("a scratch directory below the system temp directory")
    }

    /// One gate that refused, with the two numbers a rerun is sure to change
    /// handed in: the runner's own stopwatch and the duration the tool printed.
    fn refused(kind: GateKind, stdout: &str, stderr: &str, duration_ms: u64) -> GateResult {
        GateResult {
            kind,
            passed: false,
            exit_code: Some(101),
            signal: None,
            duration_ms,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            timed_out: false,
        }
    }

    /// A cargo verify run that refused, listing `failures` twice as libtest
    /// does — once as headings full of panic text, once as the summary's own
    /// list of names.
    fn refused_verify(duration_ms: u64, finished_in: &str, failures: &[&str]) -> GateResult {
        use std::fmt::Write as _;

        let mut stdout = format!("running {} tests\n", failures.len() + 1);
        for name in failures {
            writeln!(stdout, "test {name} ... FAILED")
                .expect("a String always has room for what is written into it");
        }
        stdout.push_str("test suite::passes ... ok\n\nfailures:\n");
        for name in failures {
            write!(
                stdout,
                "\n---- {name} stdout ----\nthread 'main' panicked at src/lib.rs:1:1: assertion \
                 failed: left == right\n"
            )
            .expect("a String always has room for what is written into it");
        }
        stdout.push_str("\nfailures:\n");
        for name in failures {
            writeln!(stdout, "    {name}")
                .expect("a String always has room for what is written into it");
        }
        write!(
            stdout,
            "\ntest result: FAILED. 1 passed; {} failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in {finished_in}\n",
            failures.len()
        )
        .expect("a String always has room for what is written into it");
        refused(GateKind::Verify, &stdout, "", duration_ms)
    }

    /// A gate whose command refused without naming a single test: a build or a
    /// lint, whose first line on stderr is the tool's own verdict.
    fn refused_tool(duration_ms: u64, verdict: &str) -> GateResult {
        refused(GateKind::Build, "", &format!("{verdict}\n"), duration_ms)
    }

    /// A gate that ran, was satisfied, and printed a great deal about it.
    fn passed_lint(duration_ms: u64) -> GateResult {
        GateResult {
            kind: GateKind::Lint,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms,
            stdout: String::new(),
            stderr: "Finished in 3.2s across 400 files\n".to_owned(),
            timed_out: false,
        }
    }

    /// Three runs of one failing test, recorded by a breaker of `threshold`.
    fn tripped(threshold: u32) -> (String, BreakerState) {
        let runs = [
            refused_verify(1_204, "0.41s", &["gate::tests::refuses"]),
            refused_verify(88_004, "12.05s", &["gate::tests::refuses"]),
            refused_verify(4_120_004, "1_120.9s", &["gate::tests::refuses"]),
        ];
        let mut breaker = Breaker::new(threshold);
        let mut first = String::new();
        let mut state = BreakerState::Allowed { seen: 0 };
        for (index, run) in runs.iter().enumerate() {
            let sig = signature(FailureClass::VerificationFailure, std::slice::from_ref(run));
            if index == 0 {
                first = sig.clone();
            }
            state = breaker.record(&sig);
        }
        (first, state)
    }

    /// A rule that never compiled is a normalization step that quietly stopped
    /// existing, and the failure it used to describe would then look like a
    /// different failure every time it came back.
    #[test]
    fn every_normalization_rule_compiles() {
        for rule in RULES {
            assert!(
                rule.regex().is_some(),
                "`{}` is not a regular expression",
                rule.source,
            );
        }
    }

    #[test]
    fn one_failing_test_keeps_one_signature_when_only_the_clock_moved() {
        let fast = refused_verify(1_842, "0.31s", &["remediate::tests::records_a_trip"]);
        let slow = refused_verify(
            9_121_004,
            "1_520.19s",
            &["remediate::tests::records_a_trip"],
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[fast]),
            signature(FailureClass::VerificationFailure, &[slow]),
            "one test refusing twice is one failure however long each run took",
        );
    }

    #[test]
    fn two_failing_tests_are_two_failures() {
        let alpha = refused_verify(900, "0.2s", &["gate::tests::alpha_refuses"]);
        let beta = refused_verify(900, "0.2s", &["gate::tests::beta_refuses"]);
        assert_ne!(
            signature(FailureClass::VerificationFailure, &[alpha]),
            signature(FailureClass::VerificationFailure, &[beta]),
            "two different tests must not spend each other's breaker budget",
        );
    }

    #[test]
    fn the_class_is_part_of_the_signature() {
        let gates = [refused_verify(900, "0.2s", &["gate::tests::alpha_refuses"])];
        assert_ne!(
            signature(FailureClass::VerificationFailure, &gates),
            signature(FailureClass::EnvironmentFailure, &gates),
            "the same test refusing for two different reasons earns two responses",
        );
    }

    #[test]
    fn two_instances_of_one_parametrised_test_are_one_failure() {
        let first = refused_verify(700, "0.1s", &["queue::tests::round_trips_case_1"]);
        let later = refused_verify(701, "0.1s", &["queue::tests::round_trips_case_12"]);
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[first]),
            signature(FailureClass::VerificationFailure, &[later]),
            "one body of code with two inputs is one failure",
        );
    }

    #[test]
    fn a_check_out_directory_that_moved_does_not_rename_the_failure() {
        let local = refused_tool(
            1_000,
            "error: /home/alice/wt/crates/ktask-core/src/gate.rs:41:5: unused import `std::fs`",
        );
        let worker = refused_tool(
            60_000,
            "error: /home/ci/agent-7/checkout/crates/ktask-core/src/gate.rs:412:9: unused \
             import `std::fs`",
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[local]),
            signature(FailureClass::VerificationFailure, &[worker]),
            "one file in two checkouts is one failure",
        );
    }

    #[test]
    fn a_refusal_that_only_took_longer_is_the_same_refusal() {
        let quick = refused_tool(1_000, "error: the privacy scan took 3s to decide");
        let slow = refused_tool(4_200_000, "error: the privacy scan took 4200ms to decide");
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[quick]),
            signature(FailureClass::VerificationFailure, &[slow]),
            "a duration is not part of what refused",
        );
    }

    /// A remediation attempt edits the file, so the position a tool reports moves
    /// even when the complaint is word for word the same one. Collapsing what
    /// digit-stripping leaves behind is what keeps the two one failure: `…:41:5:`
    /// and `…:7:` strip to different runs of colons, and only the last rule makes
    /// them the same word.
    #[test]
    fn the_line_a_refusal_was_reported_on_is_not_part_of_the_failure() {
        let before = refused_tool(
            1_000,
            "error: unused import `std::fs`: src/gate.rs:41:5: module never used",
        );
        let after = refused_tool(
            1_900,
            "error: unused import `std::fs`: src/gate.rs:7: module never used",
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[before]),
            signature(FailureClass::VerificationFailure, &[after]),
            "one complaint reported at two positions is one failure",
        );
    }

    /// The verdict is read from stderr because that is where a build or a lint
    /// puts it, and stdout is whatever the command happened to say while it
    /// worked. A run that narrated itself differently is the refusal it was.
    #[test]
    fn the_verdict_of_a_nameless_refusal_is_read_from_stderr() {
        let loud = refused(
            GateKind::Build,
            "warning: 12 files scanned\n",
            "error[E0308]: mismatched types\n",
            800,
        );
        let quiet = refused(
            GateKind::Build,
            "note: the cache was cold\n",
            "error[E0308]: mismatched types\n",
            44_000,
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[loud]),
            signature(FailureClass::VerificationFailure, &[quiet]),
            "two runs of one build failure that chatted differently are one failure",
        );
    }

    #[test]
    fn two_refusals_that_say_different_things_are_two_failures() {
        let types = refused_tool(1_000, "error[E0308]: mismatched types");
        let import = refused_tool(1_000, "error[E0432]: unresolved import `std::fs`");
        assert_ne!(
            signature(FailureClass::VerificationFailure, &[types]),
            signature(FailureClass::VerificationFailure, &[import]),
            "a nameless refusal must not be one signature that trips across unrelated work",
        );
    }

    #[test]
    fn the_order_two_failures_were_reported_in_changes_nothing() {
        let one = refused_verify(500, "0.5s", &["a::tests::alpha", "b::tests::beta"]);
        let other = refused_verify(500, "0.5s", &["b::tests::beta", "a::tests::alpha"]);
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[one]),
            signature(FailureClass::VerificationFailure, &[other]),
            "libtest lists its failures in whatever order they finished",
        );
    }

    #[test]
    fn two_refusing_gates_are_one_failure_whichever_ran_first() {
        let verify = refused_verify(500, "0.5s", &["a::tests::alpha"]);
        let build = refused_tool(60, "error[E0308]: mismatched types");
        assert_eq!(
            signature(
                FailureClass::VerificationFailure,
                &[verify.clone(), build.clone()],
            ),
            signature(FailureClass::VerificationFailure, &[build, verify]),
            "which gate refused first is cargo's scheduling, not the failure",
        );
    }

    #[test]
    fn one_test_that_two_binaries_reported_is_still_one_failure() {
        let single = refused_verify(500, "0.5s", &["gate::tests::refuses"]);
        let doubled = refused(
            GateKind::Verify,
            &format!("{}{}", single.stdout, single.stdout),
            "",
            4_000_000,
        );
        assert_eq!(
            signature(FailureClass::VerificationFailure, &[single]),
            signature(FailureClass::VerificationFailure, &[doubled]),
            "one test reported by two binaries has not become two failures",
        );
    }

    #[test]
    fn a_gate_that_passed_contributes_nothing() {
        let verify = refused_verify(500, "0.5s", &["gate::tests::refuses"]);
        assert_eq!(
            signature(
                FailureClass::VerificationFailure,
                std::slice::from_ref(&verify)
            ),
            signature(
                FailureClass::VerificationFailure,
                &[verify, passed_lint(72_000_000)],
            ),
            "a green gate says nothing about what refused",
        );
    }

    #[test]
    fn a_silent_refusal_is_named_by_the_gate_that_refused() {
        assert_eq!(
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Verify, "", "", 10)],
            ),
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Verify, "", "", 9_999)],
            ),
            "a gate that refused without a word still repeats itself",
        );
        assert_ne!(
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Verify, "", "", 10)],
            ),
            signature(
                FailureClass::EnvironmentFailure,
                &[refused(GateKind::Lint, "", "", 10)],
            ),
            "two gates that both refused in silence are not one failure",
        );
    }

    #[test]
    fn a_signature_is_hex_of_the_documented_length() {
        let sig = signature(FailureClass::AgentFailure, &[]);
        assert_eq!(sig.chars().count(), HEX_CHARS, "{sig} is the wrong length");
        assert!(
            sig.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{sig} is not lowercase hexadecimal",
        );
    }

    #[test]
    fn a_failure_that_named_nothing_is_told_apart_by_its_class() {
        assert_ne!(
            signature(FailureClass::AgentFailure, &[]),
            signature(FailureClass::PolicyFailure, &[]),
            "a class is part of the answer even when no gate said a word",
        );
    }

    proptest! {
        /// The stopwatch is not part of the failure, at any two readings.
        #[test]
        fn any_two_durations_of_one_refusal_are_one_failure(
            first in any::<u64>(),
            second in any::<u64>(),
        ) {
            let quick = refused_verify(first, "0.11s", &["gate::tests::refuses"]);
            let slow = refused_verify(second, "9.99s", &["gate::tests::refuses"]);
            prop_assert_eq!(
                signature(FailureClass::VerificationFailure, &[quick]),
                signature(FailureClass::VerificationFailure, &[slow]),
            );
        }

        /// No transcript, however strange, yields a signature of another shape.
        #[test]
        fn a_signature_is_always_hex(text in "[\\x20-\\x7e]{0,200}", ms in any::<u64>()) {
            let gate = refused(GateKind::Privacy, &text, &text, ms);
            let sig = signature(FailureClass::PolicyFailure, &[gate]);
            prop_assert_eq!(sig.chars().count(), HEX_CHARS, "{} is the wrong length", sig);
            prop_assert!(
                sig.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{} is not lowercase hexadecimal", sig,
            );
        }
    }

    #[test]
    fn the_breaker_trips_when_one_signature_reaches_the_threshold() {
        let mut breaker = Breaker::new(3);
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Allowed { seen: 1 }
        );
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Allowed { seen: 2 }
        );
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Tripped {
                signature: "aaaa111122223333".to_owned(),
                seen: 3,
            },
        );
    }

    #[test]
    fn another_signature_starts_a_count_of_its_own() {
        let mut breaker = Breaker::new(3);
        assert_eq!(breaker.record("one"), BreakerState::Allowed { seen: 1 });
        assert_eq!(breaker.record("two"), BreakerState::Allowed { seen: 1 });
        assert_eq!(breaker.record("one"), BreakerState::Allowed { seen: 2 });
        assert_eq!(breaker.record("two"), BreakerState::Allowed { seen: 2 });
        assert!(
            matches!(breaker.record("one"), BreakerState::Tripped { seen: 3, .. }),
            "a failure that alternates with a different one is still the same \
             failure returning",
        );
    }

    #[test]
    fn a_tripped_breaker_does_not_close_again() {
        let mut breaker = Breaker::new(1);
        assert_eq!(
            breaker.record("aaaa111122223333"),
            BreakerState::Tripped {
                signature: "aaaa111122223333".to_owned(),
                seen: 1,
            },
        );
        assert!(
            matches!(
                breaker.record("bbbb111122223333"),
                BreakerState::Tripped { .. },
            ),
            "a breaker that re-closes on a differently worded failure is a counter",
        );
    }

    #[test]
    fn three_runs_of_one_failing_test_trip_the_breaker() {
        let (first, last) = tripped(3);
        assert_eq!(
            last,
            BreakerState::Tripped {
                signature: first,
                seen: 3,
            },
            "three runs of one test that only differed by their durations are \
             three records of one signature",
        );
    }

    #[test]
    fn two_runs_of_one_failing_test_do_not_trip_a_threshold_of_three() {
        let mut breaker = Breaker::new(3);
        for run in [
            refused_verify(1_204, "0.41s", &["gate::tests::refuses"]),
            refused_verify(88_004, "12.05s", &["gate::tests::refuses"]),
        ] {
            let sig = signature(FailureClass::VerificationFailure, &[run]);
            let state = breaker.record(&sig);
            assert!(
                matches!(state, BreakerState::Allowed { seen: 1 | 2 }),
                "one failure short of the threshold is not a trip: {state:?}",
            );
        }
    }

    #[test]
    fn the_trip_is_journaled_as_the_tasks_failure() {
        let (sig, state) = tripped(3);
        let BreakerState::Tripped { signature, seen } = state else {
            panic!("three identical records must trip: {state:?}");
        };
        let dir = scratch();
        let state_dir = dir.path().join("state-7");
        std::fs::create_dir(&state_dir).expect("a state directory the journal may live in");
        let task = TaskId::new(7);
        let mut journal = Journal::open(&journal_path(&state_dir)).expect("a journal opens in it");
        journal
            .append(
                Some(task),
                &trip_event(FailureClass::VerificationFailure, &signature, seen),
            )
            .expect("a trip is written before anything else happens");

        let rows = journal.events_for(task).expect("the journal reads back");
        assert_eq!(rows.len(), 1, "one trip is one record");
        assert_eq!(rows[0].kind.discriminant(), "TaskFailed");
        let EventKind::TaskFailed { class, detail } = &rows[0].kind else {
            panic!(
                "a trip is journalled as the task's failure: {:?}",
                rows[0].kind
            );
        };
        assert_eq!(*class, FailureClass::VerificationFailure);
        assert!(
            detail.contains(&signature) && detail.contains(&seen.to_string()),
            "the record has to say which signature tripped and how often: {detail}",
        );
        assert_eq!(
            signature, sig,
            "the journal holds the signature the runs gave"
        );

        let projected = apply(
            &TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            &rows[0].kind,
        )
        .expect("a task under remediation can be failed by a trip");
        assert_eq!(
            projected,
            TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: detail.clone(),
            },
        );
    }
}

//! Vocabulary for classifying why an attempt failed, which stream a line of
//! agent output came from, how the runner recovers from an interruption,
//! and which `tdd` protocol exception a task invoked -- plus [`classify`]
//! itself, which turns a real failure into a [`FailureClass`].
//!
//! `FailureClass` is defined exactly as `docs/DESIGN.md` states it under
//! "Core types". `Stream` and `Recovery` come from the same document's event
//! catalog. `TddException` encodes the four exception categories named in
//! `VISION.md` §9 ("documentation, pure refactoring, build configuration,
//! and bugs already covered by a failing test"). All four are plain data:
//! no logic, no I/O.
//!
//! [`classify`] is the one place that logic lives. It is pure -- no I/O, no
//! provider-specific knowledge beyond matching text already captured in
//! [`Outcome`] and [`Error`] -- and total: every input reaches exactly one
//! [`FailureClass`], in the fixed priority `VISION.md` §7 lists, with
//! [`FailureClass::AgentFailure`] as the explicit fallback rather than a
//! wildcard that could quietly swallow a case nobody thought of.
//!
//! [`parse_reset`] and [`wait_plan`] implement the other half of `VISION.md`
//! §7's `provider_limit` handling: reading a reset time out of provider
//! output, then turning it -- or its absence -- into a wait strategy that is
//! never unbounded.

use crate::{Error, GateResult, Outcome};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use time::{Duration, OffsetDateTime, Time, Weekday};

/// Why an attempt or a gate failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// The agent itself failed: a non-zero exit, a crash, a refusal.
    AgentFailure,
    /// A verification gate (tests, lint, build, ...) failed.
    VerificationFailure,
    /// The provider reported a usage limit.
    ProviderLimit,
    /// The provider failed transiently and a retry may succeed.
    ProviderTransient,
    /// The provider is misconfigured (bad credentials, wrong model, ...).
    ProviderConfiguration,
    /// Publishing conflicted with mainline.
    GitConflict,
    /// The environment failed independent of the agent or the provider,
    /// for example a full disk.
    EnvironmentFailure,
    /// A policy (write scope, secret redaction, ...) was violated.
    PolicyFailure,
    /// The task is blocked on information only a human can supply.
    NeedsInput,
}

/// Which stream a line of agent output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// How the runner reconciles a task's recorded state with reality after an
/// interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovery {
    /// The interrupted work can resume where it left off.
    Resume,
    /// The interrupted work cannot be trusted and is marked interrupted.
    MarkInterrupted,
    /// The work the journal describes was already applied; nothing to redo.
    AlreadyApplied,
}

/// A recognized exception to the `tdd` protocol's red/green/refactor
/// ordering, recorded in task history whenever it is invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TddException {
    /// The change is documentation only.
    Documentation,
    /// The change is a pure refactor: behavior is unchanged and existing
    /// tests already cover it.
    PureRefactoring,
    /// The change is build configuration, not production behavior.
    BuildConfiguration,
    /// The change fixes a bug a failing test already covered before this
    /// attempt began.
    PreExistingFailingTest,
}

/// Regular expressions recognizing a provider usage-limit report without any
/// configuration: Claude's session and weekly limit messages, Codex's (and
/// other OpenAI-compatible providers') rate-limit and quota messages, and the
/// generic HTTP shape a provider that proxies through an API commonly falls
/// back to.
///
/// Compiled once and reused, since compiling a regex is too expensive to
/// repeat on every call to [`limit_message`]. Every entry here has a fixture
/// in this module's tests, matching a realistic line of provider output.
static DEFAULT_LIMIT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Claude's generic usage-limit message, e.g. "Claude AI usage limit
        // reached. Your limit will reset at 3pm (America/Los_Angeles)."
        r"(?i)usage limit",
        // Claude Code's session limit, e.g. "5-hour limit reached ∙ resets 3pm".
        r"(?i)\b5-hour limit\b",
        // Claude Code's weekly limit, e.g. "Weekly limit reached ∙ resets
        // Thursday at 12am".
        r"(?i)\bweekly limit\b",
        // Codex's (and any OpenAI-compatible API's) rate-limit message, e.g.
        // "Rate limit reached for gpt-5-codex ... Please try again in 20s."
        r"(?i)rate limit",
        // Codex's (and any OpenAI-compatible API's) quota message, e.g. "You
        // exceeded your current quota, please check your plan and billing
        // details."
        r"(?i)exceeded your current quota",
        // A generic backoff instruction providers fall back to when they do
        // not name the limit explicitly.
        r"(?i)try again later",
        // The generic HTTP status a provider that proxies through an API
        // commonly falls back to.
        r"\b429\b",
    ]
    .iter()
    // Every pattern here is exercised by this module's tests, so a broken
    // one would fail a test rather than surface here; skipping instead of
    // panicking keeps a typo in one default from taking down every other
    // default (this crate treats errors as values, never as a reason for a
    // supervisor process to panic).
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// Scans `text` line by line for one matching a default limit pattern or any
/// of `patterns` (additional regular expressions, e.g. from
/// [`crate::Config`]), returning the first matching line when found.
///
/// An entry in `patterns` that fails to compile as a regex is skipped rather
/// than aborting the scan, since a single malformed configured pattern must
/// not defeat the built-in detection (mirrors [`crate::redact()`]).
///
/// Checked by [`classify`] ahead of [`FailureClass::ProviderTransient`]: a
/// limit is not a malfunction worth an immediate retry, it is a wait --
/// sometimes with a known reset time -- so it must not be folded into the
/// generic transient bucket, and it must never be classified as
/// [`FailureClass::AgentFailure`] (`VISION.md` §7).
#[must_use]
pub fn limit_message(text: &str, patterns: &[String]) -> Option<String> {
    let extra: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();
    text.lines()
        .find(|line| {
            DEFAULT_LIMIT_PATTERNS.iter().any(|re| re.is_match(line))
                || extra.iter().any(|re| re.is_match(line))
        })
        .map(str::to_string)
}

/// Case-insensitive substrings that mark a line of agent output as the
/// agent stating it cannot proceed without a human decision (`VISION.md`
/// §7: "unresolved product or technical decision").
const NEEDS_INPUT_MARKERS: [&str; 5] = [
    "cannot proceed without",
    "need clarification",
    "needs input",
    "please clarify",
    "requires a decision",
];

/// Scans `outcome`'s stdout, then its stderr, line by line for one where
/// the agent reports it cannot proceed without a human decision, returning
/// that line when found.
#[must_use]
fn needs_input_message(outcome: &Outcome) -> Option<&str> {
    [outcome.stdout.as_str(), outcome.stderr.as_str()]
        .into_iter()
        .flat_map(str::lines)
        .find(|line| {
            let lower = line.to_lowercase();
            NEEDS_INPUT_MARKERS
                .iter()
                .any(|marker| lower.contains(marker))
        })
}

/// Case-insensitive substrings in an [`Error::Provider`] detail that mark it
/// as a configuration problem: the executable is missing or unusable, the
/// command was misconfigured, or the reported model does not match what was
/// requested (see [`crate::check_model`]) -- never something a retry could
/// fix.
const PROVIDER_CONFIGURATION_MARKERS: [&str; 7] = [
    "could not start",
    "the configured command is empty",
    "no such file or directory",
    "unauthorized",
    "authentication",
    "invalid api key",
    "does not match reported model",
];

fn is_provider_configuration_detail(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    PROVIDER_CONFIGURATION_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Classifies why an attempt failed, applying, in order, the priority
/// `VISION.md` §7 fixes: provider configuration, provider limit, provider
/// transient, git conflict, policy, verification, needs input, environment,
/// and finally [`FailureClass::AgentFailure`] as the explicit fallback. A
/// class earlier in that order is decided before a later one is even
/// considered, so an attempt that is e.g. both a provider limit and a
/// failing gate is reported as the limit -- the class that should actually
/// govern recovery.
///
/// `outcome` is what the provider reported for the attempt (its own text is
/// what [`limit_message`] and the `needs_input` check read). `gates` is
/// every completion gate that ran for the attempt. `git_error` is whatever
/// [`Error`] the surrounding attempt raised outside of `outcome` and
/// `gates`: a provider that could not even be invoked, a rejected push, a
/// dirty tree caught by policy, or a host I/O failure.
///
/// The fallback is reached only when nothing above recognized the failure.
/// In particular, a `git_error` that is `Some(Error::Provider { .. })` but
/// does not match a known configuration message never reaches it: it is
/// classified [`FailureClass::ProviderTransient`] instead, since treating an
/// unrecognized provider error as the agent's fault would send recovery
/// down the wrong path (`VISION.md` §7: "network error, temporary service
/// failure, process crash").
#[must_use]
pub fn classify(
    outcome: &Outcome,
    gates: &[GateResult],
    git_error: Option<&Error>,
) -> FailureClass {
    if let Some(Error::Provider { detail, .. }) = git_error
        && is_provider_configuration_detail(detail)
    {
        return FailureClass::ProviderConfiguration;
    }

    if limit_message(&outcome.stdout, &[])
        .or_else(|| limit_message(&outcome.stderr, &[]))
        .is_some()
    {
        return FailureClass::ProviderLimit;
    }

    if matches!(git_error, Some(Error::Provider { .. })) {
        return FailureClass::ProviderTransient;
    }

    if matches!(git_error, Some(Error::Git { .. })) {
        return FailureClass::GitConflict;
    }

    if matches!(git_error, Some(Error::Policy { .. })) {
        return FailureClass::PolicyFailure;
    }

    if gates.iter().any(|gate| !gate.passed) {
        return FailureClass::VerificationFailure;
    }

    if needs_input_message(outcome).is_some() {
        return FailureClass::NeedsInput;
    }

    if matches!(git_error, Some(Error::Io(_))) {
        return FailureClass::EnvironmentFailure;
    }

    FailureClass::AgentFailure
}

/// A relative reset like "try again in 20s" or "resets in 5 minutes": the
/// literal word "in", a count, and a unit. Unit alternatives are ordered
/// longest-name-first so a wrong alternative never has to be backtracked out
/// of before the trailing `\b` is checked.
///
/// `None` only if the static pattern itself fails to compile, which the
/// fixtures in this module's tests would catch; [`parse_reset`] treats that
/// the same as "this pattern did not match" rather than panicking (mirrors
/// [`DEFAULT_LIMIT_PATTERNS`]).
static RELATIVE_RESET: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\bin\s+(\d+)\s*(seconds?|secs?|s|minutes?|mins?|m|hours?|hrs?|h|days?|d)\b")
        .ok()
});

/// An absolute clock-time reset like "resets 3pm", "reset at 12am", or
/// "resets Thursday at 12am": an optional weekday name, an hour, an optional
/// `:MM`, and an am/pm marker. See [`RELATIVE_RESET`] for why this is an
/// `Option`.
static ABSOLUTE_RESET: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:(sunday|monday|tuesday|wednesday|thursday|friday|saturday)[a-z]*\s+(?:at\s+)?)?(\d{1,2})(?::(\d{2}))?\s*([ap]m)\b",
    )
    .ok()
});

fn parse_weekday(name: &str) -> Option<Weekday> {
    match name.to_ascii_lowercase().as_str() {
        "sunday" => Some(Weekday::Sunday),
        "monday" => Some(Weekday::Monday),
        "tuesday" => Some(Weekday::Tuesday),
        "wednesday" => Some(Weekday::Wednesday),
        "thursday" => Some(Weekday::Thursday),
        "friday" => Some(Weekday::Friday),
        "saturday" => Some(Weekday::Saturday),
        _ => None,
    }
}

/// Parses a known reset time out of a line of provider output: a relative
/// duration ("try again in 20s") or an absolute wall-clock time ("resets
/// 3pm", "resets Thursday at 12am"), read against the offset carried by
/// `now`.
///
/// A bare or weekday-qualified clock time that has already passed rolls
/// forward -- to the same time tomorrow, or to the next occurrence of the
/// named weekday -- rather than resolving into the past. This is what makes
/// a reset reported just before midnight land on the following calendar day:
/// "resets 12am" parsed at 23:59:30 is thirty seconds away, not almost a
/// full day in the past.
///
/// Returns `None` when `text` contains no reset expression this function
/// recognizes. [`wait_plan`] turns that into a bounded backoff rather than
/// an unbounded wait, per `VISION.md` §7.
#[must_use]
pub fn parse_reset(text: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    if let Some(caps) = RELATIVE_RESET.as_ref().and_then(|re| re.captures(text)) {
        let count: i64 = caps.get(1)?.as_str().parse().ok()?;
        let unit = caps.get(2)?.as_str();
        let delta = match unit.chars().next()?.to_ascii_lowercase() {
            'h' => Duration::hours(count),
            'm' => Duration::minutes(count),
            'd' => Duration::days(count),
            _ => Duration::seconds(count),
        };
        return Some(now + delta);
    }

    let caps = ABSOLUTE_RESET.as_ref()?.captures(text)?;

    let hour: u8 = caps.get(2)?.as_str().parse().ok()?;
    if !(1..=12).contains(&hour) {
        return None;
    }
    let minute: u8 = match caps.get(3) {
        Some(m) => m.as_str().parse().ok()?,
        None => 0,
    };
    if minute > 59 {
        return None;
    }
    let is_pm = caps.get(4)?.as_str().eq_ignore_ascii_case("pm");
    let hour24 = match (hour, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, false) => h,
        (h, true) => h + 12,
    };
    let clock = Time::from_hms(hour24, minute, 0).ok()?;

    let weekday = caps.get(1);
    let target_date = match weekday {
        Some(name) => {
            let target = parse_weekday(name.as_str())?;
            let forward = (i64::from(target.number_days_from_monday())
                - i64::from(now.weekday().number_days_from_monday()))
            .rem_euclid(7);
            now.date() + Duration::days(forward)
        }
        None => now.date(),
    };

    let mut candidate = target_date.with_time(clock).assume_offset(now.offset());
    if candidate <= now {
        candidate += Duration::days(if weekday.is_some() { 7 } else { 1 });
    }
    Some(candidate)
}

/// What to do while a [`FailureClass::ProviderLimit`] is outstanding:
/// returned by [`wait_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPlan {
    /// Wait until this exact instant -- the parsed reset plus margin.
    Deadline(OffsetDateTime),
    /// No usable reset time: wait this long, bounded by `max`, then
    /// re-check.
    Backoff(Duration),
}

/// Turns a possibly-known reset time into a wait strategy, per `VISION.md`
/// §7: "`provider_limit` with a known reset waits until the exact reset
/// time, with margin ...; unknown resets use bounded backoff."
///
/// `reset` is normally [`parse_reset`]'s output. When it names an instant
/// still ahead of `now`, the plan is [`WaitPlan::Deadline`] at `reset +
/// margin` -- the margin absorbs clock skew between this host and the
/// provider so an attempt does not resume a moment before the provider
/// actually lifts the limit. Anything else -- no reset recognized, or one
/// that has already passed `now` (clock skew the other way, or a limit that
/// lifted between the check and this call) -- is [`WaitPlan::Backoff`]
/// capped at `max`, never an unbounded wait.
///
/// This function takes no jitter parameter: it is pure and deterministic, so
/// retries stay reproducible in tests. A caller wanting the jitter `VISION.md`
/// §7 also asks for layers it onto the returned deadline or backoff.
#[must_use]
pub fn wait_plan(
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
    margin: Duration,
    max: Duration,
) -> WaitPlan {
    match reset {
        Some(at) if at > now => WaitPlan::Deadline(at + margin),
        _ => WaitPlan::Backoff(max),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_failure_classes() -> Vec<FailureClass> {
        vec![
            FailureClass::AgentFailure,
            FailureClass::VerificationFailure,
            FailureClass::ProviderLimit,
            FailureClass::ProviderTransient,
            FailureClass::ProviderConfiguration,
            FailureClass::GitConflict,
            FailureClass::EnvironmentFailure,
            FailureClass::PolicyFailure,
            FailureClass::NeedsInput,
        ]
    }

    #[test]
    fn failure_class_has_exactly_nine_variants() {
        let variants = all_failure_classes();
        assert_eq!(variants.len(), 9);

        // Exhaustive, wildcard-free match: a variant added to `FailureClass`
        // without being listed here fails to compile instead of silently
        // under-counting.
        for class in variants {
            match class {
                FailureClass::AgentFailure
                | FailureClass::VerificationFailure
                | FailureClass::ProviderLimit
                | FailureClass::ProviderTransient
                | FailureClass::ProviderConfiguration
                | FailureClass::GitConflict
                | FailureClass::EnvironmentFailure
                | FailureClass::PolicyFailure
                | FailureClass::NeedsInput => {}
            }
        }
    }

    #[test]
    fn every_failure_class_round_trips_through_json() {
        for class in all_failure_classes() {
            let json = serde_json::to_string(&class).expect("serialize");
            let back: FailureClass = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(class, back);
        }
    }

    fn all_streams() -> Vec<Stream> {
        vec![Stream::Stdout, Stream::Stderr]
    }

    #[test]
    fn stream_has_exactly_two_variants() {
        let variants = all_streams();
        assert_eq!(variants.len(), 2);

        for stream in variants {
            match stream {
                Stream::Stdout | Stream::Stderr => {}
            }
        }
    }

    #[test]
    fn every_stream_round_trips_through_json() {
        for stream in all_streams() {
            let json = serde_json::to_string(&stream).expect("serialize");
            let back: Stream = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(stream, back);
        }
    }

    fn all_recoveries() -> Vec<Recovery> {
        vec![
            Recovery::Resume,
            Recovery::MarkInterrupted,
            Recovery::AlreadyApplied,
        ]
    }

    #[test]
    fn recovery_has_exactly_three_variants() {
        let variants = all_recoveries();
        assert_eq!(variants.len(), 3);

        for recovery in variants {
            match recovery {
                Recovery::Resume | Recovery::MarkInterrupted | Recovery::AlreadyApplied => {}
            }
        }
    }

    #[test]
    fn every_recovery_round_trips_through_json() {
        for recovery in all_recoveries() {
            let json = serde_json::to_string(&recovery).expect("serialize");
            let back: Recovery = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(recovery, back);
        }
    }

    fn all_tdd_exceptions() -> Vec<TddException> {
        vec![
            TddException::Documentation,
            TddException::PureRefactoring,
            TddException::BuildConfiguration,
            TddException::PreExistingFailingTest,
        ]
    }

    #[test]
    fn tdd_exception_has_exactly_four_variants() {
        let variants = all_tdd_exceptions();
        assert_eq!(variants.len(), 4);

        for exception in variants {
            match exception {
                TddException::Documentation
                | TddException::PureRefactoring
                | TddException::BuildConfiguration
                | TddException::PreExistingFailingTest => {}
            }
        }
    }

    #[test]
    fn every_tdd_exception_round_trips_through_json() {
        for exception in all_tdd_exceptions() {
            let json = serde_json::to_string(&exception).expect("serialize");
            let back: TddException = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(exception, back);
        }
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;
    use crate::GateKind;
    use std::io;
    use std::path::PathBuf;

    fn empty_outcome() -> Outcome {
        Outcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
            usage: None,
            session_id: None,
        }
    }

    fn passed_gate(kind: GateKind) -> GateResult {
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

    /// A missing executable, exactly the shape `provider/claude.rs`'s
    /// `invoking_a_missing_executable_is_a_provider_error` test proves
    /// `Claude::invoke` produces.
    #[test]
    fn a_missing_executable_is_provider_configuration() {
        let outcome = empty_outcome();
        let err = Error::Provider {
            provider: "ktask-claude-test-nonexistent-binary".to_string(),
            detail: "could not start `ktask-claude-test-nonexistent-binary`: \
                      No such file or directory (os error 2)"
                .to_string(),
        };
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::ProviderConfiguration
        );
    }

    /// The exact detail message `check_model` in `provider/mod.rs` produces
    /// for a reported/configured model mismatch.
    #[test]
    fn a_reported_model_mismatch_is_provider_configuration() {
        let outcome = empty_outcome();
        let err = Error::Provider {
            provider: "model".to_string(),
            detail: "configured model \"claude-opus-5\" does not match reported model \
                      \"claude-sonnet-5\""
                .to_string(),
        };
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::ProviderConfiguration
        );
    }

    #[test]
    fn a_usage_limit_line_in_stdout_is_provider_limit() {
        let mut outcome = empty_outcome();
        outcome.stdout = "Claude AI usage limit reached. Your limit will reset at 3pm.".to_string();
        assert_eq!(classify(&outcome, &[], None), FailureClass::ProviderLimit);
    }

    /// The core of "never silently absorbs an unrecognised provider error":
    /// a detail this classifier has never seen still lands on a provider
    /// class, not on the agent-failure fallback that would misdirect
    /// recovery at the agent instead of the provider.
    #[test]
    fn an_unrecognized_provider_error_is_provider_transient_not_agent_failure() {
        let outcome = empty_outcome();
        let err = Error::Provider {
            provider: "claude".to_string(),
            detail: "idle timeout of 300s exceeded with no output".to_string(),
        };
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::ProviderTransient
        );
    }

    /// The exact shape `git.rs`'s
    /// `publish_surfaces_a_rejected_push_distinctly_from_a_mismatch` test
    /// proves a rejected push takes.
    #[test]
    fn a_rejected_push_is_a_git_conflict() {
        let outcome = empty_outcome();
        let err = Error::Git {
            args: vec!["push".to_string(), "origin".to_string(), "main".to_string()],
            stderr: "! [rejected]        HEAD -> main (non-fast-forward)".to_string(),
        };
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::GitConflict
        );
    }

    #[test]
    fn a_dirty_tree_is_a_policy_failure() {
        let outcome = empty_outcome();
        let err = Error::Policy {
            detail: "dirty working tree before verify".to_string(),
            paths: vec![PathBuf::from("src/lib.rs")],
        };
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::PolicyFailure
        );
    }

    #[test]
    fn a_failing_verify_gate_is_a_verification_failure() {
        let outcome = empty_outcome();
        let gates = vec![GateResult {
            passed: false,
            exit_code: Some(1),
            stderr: "test result: FAILED. 2 passed; 1 failed;".to_string(),
            ..passed_gate(GateKind::Verify)
        }];
        assert_eq!(
            classify(&outcome, &gates, None),
            FailureClass::VerificationFailure
        );
    }

    #[test]
    fn an_explicit_request_for_a_decision_is_needs_input() {
        let mut outcome = empty_outcome();
        outcome.stdout =
            "I cannot proceed without knowing which database driver to target.".to_string();
        assert_eq!(classify(&outcome, &[], None), FailureClass::NeedsInput);
    }

    #[test]
    fn a_disk_io_failure_is_an_environment_failure() {
        let outcome = empty_outcome();
        let err = Error::Io(io::Error::other("No space left on device"));
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::EnvironmentFailure
        );
    }

    #[test]
    fn a_plain_nonzero_exit_with_nothing_else_recognized_is_agent_failure() {
        let mut outcome = empty_outcome();
        outcome.stderr = "assertion `left == right` failed\n  left: 3\n right: 4".to_string();
        assert_eq!(classify(&outcome, &[], None), FailureClass::AgentFailure);
    }

    #[test]
    fn provider_configuration_outranks_a_simultaneous_usage_limit() {
        let mut outcome = empty_outcome();
        outcome.stdout = "rate limit exceeded".to_string();
        let err = Error::Provider {
            provider: "claude".to_string(),
            detail: "the configured command is empty".to_string(),
        };
        assert_eq!(
            classify(&outcome, &[], Some(&err)),
            FailureClass::ProviderConfiguration
        );
    }

    #[test]
    fn a_verify_gate_failure_outranks_an_unrelated_needs_input_line() {
        // Ordering: verification is checked before needs_input, so a real
        // gate failure is reported even when the agent's own transcript
        // also contains a needs-input phrase.
        let mut outcome = empty_outcome();
        outcome.stdout = "please clarify the target platform before I continue".to_string();
        let gates = vec![GateResult {
            passed: false,
            exit_code: Some(1),
            ..passed_gate(GateKind::Verify)
        }];
        assert_eq!(
            classify(&outcome, &gates, None),
            FailureClass::VerificationFailure
        );
    }

    #[test]
    fn limit_message_finds_a_rate_limit_line_in_stderr() {
        let outcome_stderr = "warming up\nrate limited, retry after 30m\n";
        assert_eq!(
            limit_message(outcome_stderr, &[]),
            Some("rate limited, retry after 30m".to_string())
        );
    }

    #[test]
    fn limit_message_returns_none_for_an_ordinary_failure() {
        let text = "panicked at src/main.rs:12: index out of bounds";
        assert_eq!(limit_message(text, &[]), None);
    }

    /// One fixture per entry in `DEFAULT_LIMIT_PATTERNS`, each drawn from a
    /// realistic line of Claude or Codex provider output. Failing to match
    /// any of these must never happen silently: a regression here is exactly
    /// the "limit classified as agent failure" bug this function exists to
    /// prevent.
    fn default_limit_fixtures() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "claude usage limit",
                "Claude AI usage limit reached. Your limit will reset at 3pm \
                 (America/Los_Angeles).",
            ),
            (
                "claude 5-hour session limit",
                "5-hour limit reached \u{2219} resets 3pm",
            ),
            (
                "claude weekly limit",
                "Weekly limit reached \u{2219} resets Thursday at 12am",
            ),
            (
                "codex rate limit",
                "Rate limit reached for gpt-5-codex in organization org-abc123 \
                 on requests per min (RPM): Limit 3, Used 3, Requested 1. \
                 Please try again in 20s.",
            ),
            (
                "codex quota exceeded",
                "You exceeded your current quota, please check your plan and \
                 billing details.",
            ),
            (
                "generic try-again-later backoff",
                "Service unavailable, please try again later.",
            ),
            (
                "generic HTTP 429 fallback",
                "request failed with status 429",
            ),
        ]
    }

    #[test]
    fn every_default_limit_pattern_matches_its_fixture() {
        for (name, fixture) in default_limit_fixtures() {
            assert_eq!(
                limit_message(fixture, &[]),
                Some(fixture.to_string()),
                "expected default patterns to match the {name} fixture: {fixture:?}"
            );
        }
    }

    /// The other half of "a limit is never classified as an agent failure":
    /// every default fixture, handed to the full classifier as the agent's
    /// stdout with nothing else going wrong, must land on
    /// [`FailureClass::ProviderLimit`], never [`FailureClass::AgentFailure`].
    #[test]
    fn every_default_limit_fixture_classifies_as_provider_limit() {
        for (name, fixture) in default_limit_fixtures() {
            let mut outcome = empty_outcome();
            outcome.stdout = fixture.to_string();
            assert_eq!(
                classify(&outcome, &[], None),
                FailureClass::ProviderLimit,
                "expected the {name} fixture to classify as a provider limit: {fixture:?}"
            );
        }
    }

    #[test]
    fn limit_message_matches_a_configured_extra_pattern() {
        let text = "internal-provider: session budget exhausted for account acme-42";
        assert_eq!(
            limit_message(text, &["budget exhausted".to_string()]),
            Some(text.to_string())
        );
    }

    #[test]
    fn an_invalid_configured_pattern_is_skipped_without_panicking() {
        let text = "value that is not itself limit shaped";
        assert_eq!(limit_message(text, &["(unclosed".to_string()]), None);
    }

    #[test]
    fn default_patterns_still_apply_when_a_configured_pattern_is_invalid() {
        let text = "usage limit reached, try again tomorrow";
        assert_eq!(
            limit_message(text, &["(unclosed".to_string()]),
            Some(text.to_string())
        );
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn parse_reset_reads_a_relative_duration_in_seconds() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let text = "Rate limit reached for gpt-5-codex ... Please try again in 20s.";
        assert_eq!(parse_reset(text, now), Some(now + Duration::seconds(20)));
    }

    #[test]
    fn parse_reset_reads_a_relative_duration_in_minutes() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let text = "rate limited, retry after resets in 5 minutes";
        assert_eq!(parse_reset(text, now), Some(now + Duration::minutes(5)));
    }

    #[test]
    fn parse_reset_reads_an_absolute_clock_time_still_ahead_today() {
        let now = datetime!(2024-01-01 14:00:00 UTC);
        let text = "5-hour limit reached \u{2219} resets 3pm";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-01 15:00:00 UTC))
        );
    }

    #[test]
    fn parse_reset_reads_an_absolute_clock_time_with_a_timezone_annotation() {
        let now = datetime!(2024-01-01 09:00:00 UTC);
        let text =
            "Claude AI usage limit reached. Your limit will reset at 3pm (America/Los_Angeles).";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-01 15:00:00 UTC))
        );
    }

    /// The day-boundary case: a clock time that has already passed today
    /// must roll forward across midnight to the same time tomorrow, not
    /// resolve to a moment already in the past.
    #[test]
    fn parse_reset_rolls_an_already_passed_clock_time_across_midnight() {
        let now = datetime!(2024-01-01 23:50:00 UTC);
        let text = "5-hour limit reached \u{2219} resets 3pm";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-02 15:00:00 UTC))
        );
    }

    /// Exactly at the day boundary: "resets 12am" parsed thirty seconds
    /// before midnight resolves thirty seconds into the next calendar day,
    /// not almost a full day in the past.
    #[test]
    fn parse_reset_at_the_day_boundary_resolves_to_the_next_calendar_day() {
        let now = datetime!(2024-01-01 23:59:30 UTC);
        let text = "resets 12am";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-02 00:00:00 UTC))
        );
    }

    #[test]
    fn parse_reset_reads_noon_as_twelve_pm() {
        let now = datetime!(2024-01-01 09:00:00 UTC);
        let text = "resets 12pm";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-01 12:00:00 UTC))
        );
    }

    /// 2024-01-04 is a Thursday. The weekly limit's reset already passed
    /// today (it's 1am, past midnight), so this rolls to *next* Thursday,
    /// crossing a full week, not just a day.
    #[test]
    fn parse_reset_weekday_reset_already_passed_today_rolls_to_next_week() {
        let now = datetime!(2024-01-04 01:00:00 UTC);
        let text = "Weekly limit reached \u{2219} resets Thursday at 12am";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-11 00:00:00 UTC))
        );
    }

    /// 2024-01-01 is a Monday; the named weekday (Thursday) is still ahead
    /// in the same week, so no rollover is needed.
    #[test]
    fn parse_reset_weekday_still_ahead_in_the_same_week() {
        let now = datetime!(2024-01-01 09:00:00 UTC);
        let text = "resets Thursday at 12am";
        assert_eq!(
            parse_reset(text, now),
            Some(datetime!(2024-01-04 00:00:00 UTC))
        );
    }

    #[test]
    fn parse_reset_returns_none_for_text_without_a_reset_expression() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let text = "panicked at src/main.rs:12: index out of bounds";
        assert_eq!(parse_reset(text, now), None);
    }

    #[test]
    fn parse_reset_rejects_an_out_of_range_hour() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        assert_eq!(parse_reset("resets 13pm", now), None);
    }

    #[test]
    fn wait_plan_waits_for_the_exact_reset_plus_margin_when_the_reset_is_known_and_ahead() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let reset = datetime!(2024-01-01 15:00:00 UTC);
        let margin = Duration::seconds(60);
        let max = Duration::hours(24);
        assert_eq!(
            wait_plan(Some(reset), now, margin, max),
            WaitPlan::Deadline(reset + margin)
        );
    }

    #[test]
    fn wait_plan_backs_off_bounded_by_max_when_no_reset_was_recognized() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let max = Duration::hours(24);
        assert_eq!(
            wait_plan(None, now, Duration::seconds(60), max),
            WaitPlan::Backoff(max)
        );
    }

    /// A reset that has already elapsed by `now` -- clock skew, or a limit
    /// that lifted between the check and this call -- must never produce a
    /// deadline in the past; it falls back to the same bounded backoff as an
    /// unrecognized reset.
    #[test]
    fn wait_plan_backs_off_rather_than_waiting_on_a_reset_already_in_the_past() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let reset = datetime!(2024-01-01 11:00:00 UTC);
        let max = Duration::hours(24);
        assert_eq!(
            wait_plan(Some(reset), now, Duration::seconds(60), max),
            WaitPlan::Backoff(max)
        );
    }

    #[test]
    fn wait_plan_treats_a_reset_exactly_at_now_as_already_past() {
        let now = datetime!(2024-01-01 12:00:00 UTC);
        let max = Duration::hours(24);
        assert_eq!(
            wait_plan(Some(now), now, Duration::seconds(60), max),
            WaitPlan::Backoff(max)
        );
    }
}

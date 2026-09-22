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

use crate::{Error, GateResult, Outcome};
use serde::{Deserialize, Serialize};

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

/// Case-insensitive substrings that mark a line of provider output as
/// reporting a usage limit rather than an ordinary failure: Claude's and
/// Codex's own limit messages, and the generic HTTP shape a provider that
/// proxies through an API commonly falls back to.
const LIMIT_MARKERS: [&str; 6] = [
    "usage limit",
    "rate limit",
    "quota exceeded",
    "limit reached",
    "try again later",
    "429",
];

/// Scans `outcome`'s stdout, then its stderr, line by line for one
/// reporting a provider usage limit, returning that line when found.
///
/// Checked by [`classify`] ahead of [`FailureClass::ProviderTransient`]: a
/// limit is not a malfunction worth an immediate retry, it is a wait --
/// sometimes with a known reset time -- so it must not be folded into the
/// generic transient bucket (`VISION.md` §7).
#[must_use]
pub fn limit_message(outcome: &Outcome) -> Option<&str> {
    [outcome.stdout.as_str(), outcome.stderr.as_str()]
        .into_iter()
        .flat_map(str::lines)
        .find(|line| {
            let lower = line.to_lowercase();
            LIMIT_MARKERS.iter().any(|marker| lower.contains(marker))
        })
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

    if limit_message(outcome).is_some() {
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
        let mut outcome = empty_outcome();
        outcome.stderr = "warming up\nrate limited, retry after 30m\n".to_string();
        assert_eq!(
            limit_message(&outcome),
            Some("rate limited, retry after 30m")
        );
    }

    #[test]
    fn limit_message_returns_none_for_an_ordinary_failure() {
        let mut outcome = empty_outcome();
        outcome.stderr = "panicked at src/main.rs:12: index out of bounds".to_string();
        assert_eq!(limit_message(&outcome), None);
    }
}

//! The names for why work stopped making progress.
//!
//! A failure reaches the supervisor as a class before it reaches anyone as a
//! sentence: what happens next — retry, wait for a reset, pause for a human,
//! stop the run — is chosen from the class, never from the text that came with
//! it. That is why the class is one closed enum shared by the journal, the
//! failures screen and `--json` output, rather than a string each reporter
//! invents.
//!
//! The reading lives here too. [`classify`] decides which of the nine names an
//! observed failure earns, and [`limit_message`] answers the one question the
//! taxonomy turns on: is this a usage limit, and on which line was it named?
//! The order the arms are applied in *is* the policy, so it is written out in
//! full and reviewed as a whole — a run with an unusable configuration pauses
//! before anything is retried, a limit is waited out before a gate is read, and
//! the work itself is the last thing a failure can be, never the default it
//! falls into. ADR-0059 records why each arm is allowed to read only the
//! evidence that belongs to it.
//!
//! [`FailureClass`] is exactly the nine variants `docs/DESIGN.md` fixes, and
//! [`TddException`] is the four exception categories VISION.md §9 allows a task
//! to claim against test-first discipline; `docs/DESIGN.md` names the type as
//! the `TddExceptionUsed` payload without listing its variants, so
//! `docs/adr/0010-tdd-exception-categories-come-from-vision-md.md` records
//! where these four came from.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::{Error, GateResult, Outcome};

/// Why a task did not get done, as the supervisor decides on it.
///
/// The nine classes partition by *what a correct response is*, not by what
/// noticed the problem: VISION.md §7 fixes the response for each — a
/// [`FailureClass::ProviderLimit`] with a known reset is waited out to the
/// exact instant, a [`FailureClass::ProviderConfiguration`] or
/// [`FailureClass::NeedsInput`] never loops at all and pauses for a human
/// immediately, and a [`FailureClass::PolicyFailure`] stops the run. Adding a
/// variant means adding a response, so the tests pin the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// The agent could not complete the implementation.
    AgentFailure,
    /// Tests, lint, build, or the privacy checks failed.
    VerificationFailure,
    /// A usage limit, with a reset time known or unknown.
    ProviderLimit,
    /// A network error, a temporary service failure, or a provider process that
    /// crashed: the same work may succeed if asked again.
    ProviderTransient,
    /// Authentication, an invalid model, a missing executable. No retry can
    /// fix this, so nothing is retried.
    ProviderConfiguration,
    /// Branch drift, a rejected push, or a publication that conflicts with what
    /// the remote already holds.
    GitConflict,
    /// A missing SDK, dependency, or host capability: the machine is not the one
    /// the task was written for.
    EnvironmentFailure,
    /// A forbidden file was touched, the tree was dirty at verification time, or
    /// a gate was bypassed.
    PolicyFailure,
    /// A product or technical decision no one has made. The agent is not
    /// authorised to make it, and the supervisor is not either.
    NeedsInput,
}

/// The one legitimate reason a task was done without tests written first.
///
/// Test-first order cannot be proved after the fact, so the `tdd` protocol
/// enforces it (VISION.md §9) and a task that genuinely does not fit applies
/// for one of these four categories instead. The point of naming them is that
/// the override is *recorded and visible* rather than silent: an exception is
/// an entry in task history with a reason beside it, which is what makes
/// "no logic without a failing test" enforceable instead of aspirational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TddException {
    /// Documentation changed; there is no behaviour for a test to pin.
    Documentation,
    /// The behaviour is unchanged, so the coverage that exists already holds.
    PureRefactoring,
    /// Build configuration changed, which the build and the gates verify.
    BuildConfiguration,
    /// A bug a failing test already reproduces: the red step arrived with the
    /// report rather than waiting to be written.
    ExistingFailingTest,
}

/// Phrases that name one class, compiled the first time the table is read.
///
/// A classifier is read far more often than it is edited, and one run reads the
/// same refusal for the journal, the failures screen and `--json` output, so a
/// table is compiled once per process behind a [`OnceLock`] rather than once per
/// call. A phrase that does not compile is skipped rather than panicking, and
/// the test named `every_table_compiles` is what stops that from being a hole in
/// the taxonomy only a misclassified run would notice.
struct Table {
    /// The phrases as they are written in this file.
    phrases: &'static [&'static str],
    /// The same phrases compiled, on first use.
    compiled: OnceLock<Vec<Regex>>,
}

impl Table {
    /// A table of `phrases`, nothing compiled yet.
    const fn new(phrases: &'static [&'static str]) -> Self {
        Self {
            phrases,
            compiled: OnceLock::new(),
        }
    }

    /// Whether `text` holds any of the phrases.
    fn matches(&self, text: &str) -> bool {
        self.compiled().iter().any(|phrase| phrase.is_match(text))
    }

    /// The compiled phrases, in the order they were written.
    fn compiled(&self) -> &[Regex] {
        self.compiled.get_or_init(|| {
            self.phrases
                .iter()
                .filter_map(|phrase| Regex::new(phrase).ok())
                .collect()
        })
    }
}

/// A command that exited 126 never ran: the file was not executable.
const NOT_EXECUTABLE: i32 = 126;

/// A command that exited 127 never ran: the name resolved to nothing.
const NOT_FOUND: i32 = 127;

/// The refusals no retry can fix, because the fault is in what was configured.
///
/// The first half are the CLIs' own words; the last four are this project's
/// refusals from ADR-0054 and ADR-0057, which is what lets a missing executable
/// and a mismatched model id reach the class that pauses for a human rather
/// than the class that spends a retry budget on them.
static CONFIGURATION: Table = Table::new(&[
    r"(?i)authentication[ _-]?error",
    r"(?i)\bunauthori[sz]ed\b",
    r"(?i)\binvalid\b[^\n]{0,40}\b(?:api[ _-]?key|apikey|key|token|credential)\b",
    r"(?i)\b(?:expired|revoked)\b[^\n]{0,40}\b(?:key|token|session|credential)\b",
    r"(?i)\b(?:log|sign)[ _-]?in\b",
    r"(?i)\blogged[ _-]?out\b",
    concat!(
        r"(?i)\b(?:unknown|invalid|unsupported|unavailable)\b",
        r"[^\n]{0,40}\bmodel\b",
    ),
    concat!(
        r"(?i)\bmodel\b[^\n]{0,40}\b(?:does not exist|no longer (?:exists|available)",
        r"|not (?:found|available|supported|valid|configured)|unknown|invalid|unavailable)\b",
    ),
    r"(?i)no command is configured",
    r"(?i)is not an executable",
    r"(?i)nothing was started",
    r"(?i)no retry can start",
]);

/// The words a usage limit is named with, before an operator configures more.
///
/// Deliberately short: the Claude and Codex wordings belong to the task that
/// owns usage-limit detection together with the `limit_patterns` key that will
/// feed `patterns` of [`limit_message`], and a table that guessed at them would
/// be edited the first time a real CLI contradicted it.
static LIMIT: Table = Table::new(&[
    r"(?i)\b(?:usage|rate|quota|credit|plan|billing|request)[ _-]?limits?\b",
    r"(?i)\blimits?\b[^\n]{0,24}\b(?:reached|exceeded|hit|applied)\b",
    r"(?i)\b(?:hit|reached|exceeded|over)\b[^\n]{0,24}\blimits?\b",
    r"(?i)\b429\b",
    r"(?i)too many requests",
    r"(?i)\bquota\b[^\n]{0,24}\b(?:exhausted|exceeded|reached|depleted)\b",
    r"(?i)insufficient[^\n]{0,24}\b(?:quota|credit|balance|funds)\b",
    r"(?i)credit balance[^\n]{0,24}\b(?:too low|exhausted|depleted)\b",
    r"(?i)\bretry after\b",
]);

/// The provider faults a fresh attempt may outlive.
///
/// "Process crash" in VISION.md §7's table is here too: a session killed by a
/// signal, or one that reported no status at all, says nothing about the work
/// and everything about the run.
static TRANSIENT: Table = Table::new(&[
    r"(?i)connection (?:reset|refused|closed|aborted|interrupted|timed out|failed)",
    r"(?i)\be(?:conn(?:reset|refused|aborted)|timedout|hostunreach|netunreach|hostdown)\b",
    r"(?i)broken pipe",
    r"(?i)stream disconnected",
    r"(?i)\b(?:timed out|timeout)\b",
    r"(?i)internal server error",
    r"(?i)bad gateway",
    r"(?i)service unavailable",
    r"(?i)temporarily unavailable",
    r"(?i)\boverloaded\b",
    r"(?i)segmentation fault",
    r"(?i)core dumped",
    r"(?i)out of memory",
    r"(?i)was terminated by signal",
    r"(?i)reported no exit status",
    r"(?i)neither a code nor a signal",
]);

/// The words that say a rule was broken rather than a check failed.
///
/// Read from what a gate printed, because a gate is what enforces a rule: the
/// agent's own prose about being blocked is an account of the rule, not
/// evidence of it, and VISION.md §3's invariant 4 keeps accounts out of the
/// evidence.
static POLICY: Table = Table::new(&[
    r"(?i)\bforbidden\b",
    r"(?i)\b(?:disallowed|prohibited)\b",
    r"(?i)not allowed",
    r"(?i)outside the write scope",
    r"(?i)\b(?:dirty tree|uncommitted (?:work|changes)|untracked file)\b",
    r"(?i)\bgate\b[^\n]{0,30}\b(?:bypassed?|skipped|disabled|suppressed)\b",
    r"(?i)\b(?:bypassed?|skipped|disabled|suppressed)\b[^\n]{0,30}\bgate\b",
    r"(?i)--no-verify",
    r"(?i)policy violation",
]);

/// What a host says when it is not the machine the task was written for.
///
/// A missing SDK is usually a gate's or a CLI's own words; the `os error` forms
/// are how this project's own refusals report the same thing.
static ENVIRONMENT: Table = Table::new(&[
    r"(?i)command not found",
    r"(?i)executable file not found",
    r"(?i)\bis not installed\b",
    r"(?i)missing (?:an? |the )?(?:sdk|toolchain|compiler|linker|dependency|library|header file)",
    r"(?i)cannot open shared object file",
    r"(?i)read[- ]only file system",
    r"(?i)no space left on device",
    r"(?i)too many open files",
    r"(?i)cannot allocate memory",
    r"(?i)text file busy",
    r"(?i)permission denied",
    r"(?i)\(os error (?:2|13|20|28|36)\)",
]);

/// The provider faults no phrase above names.
///
/// This is the net under the fallback, and the reason the fallback stays
/// honest: an unrecognised refusal of the provider is retried within bounds,
/// whereas reading it as the agent's own failure spends a remediation on a
/// machine nobody was blaming. It is narrow on purpose — a noun that says who
/// refused, beside a word that says it refused — because a table that matched
/// any complaint would absorb every agent failure into a retry.
static PROVIDER_RESIDUAL: Table = Table::new(&[
    concat!(
        r"(?i)\b(?:api|provider|upstream|gateway|endpoint|anthropic|openai|claude|codex|model)s?\b",
        r"[^\n]{0,60}\b(?:error[s]?|fail|fails|failed|failing|failure|refus(?:e[sd]?|ing|al)",
        r"|unreachable|unavailable|unreadable|malformed|timed out|timeout|faults?",
        r"|crash(?:es|ed|ing)?|died|5\d\d)\b",
    ),
    r"(?i)\b(?:status|code) 5\d\d\b",
    r"(?i)\b(?:retry|try again) later\b",
]);

/// What `git.rs` writes when there is no `git` to run, as opposed to a `git`
/// that ran and refused.
static GIT_UNSTARTED: Table = Table::new(&[r"(?i)could not be started"]);

/// A report line asking for a decision, anchored at the start of a line.
///
/// The anchor is the rule: a session quoting its own prompt, or a template
/// describing this very line, is prose about the protocol rather than a use of
/// it, and a run that paused on every echo of the prompt would never finish.
static NEEDS_INPUT: Table = Table::new(&[r"(?im)^ktask_result:[ \t]*needs_input\b"]);

/// Sort an observed failure into the class that says what to do next.
///
/// The order the arms are applied in *is* the policy, and VISION.md §7 relies
/// on it being fixed: provider configuration, provider limit, provider
/// transient, git conflict, policy, verification, needs input, environment, an
/// unrecognised provider fault, and only then [`FailureClass::AgentFailure`].
/// Reading a failure one arm off is the expensive mistake — a limit read as an
/// [`FailureClass::AgentFailure`] spends the remediation budget on what is a
/// wait, and a configuration mistake read as a
/// [`FailureClass::ProviderTransient`] loops until the circuit breaker trips.
///
/// # What each arm is allowed to read
///
/// [`Outcome`]'s text is read differently depending on what it is being asked.
/// A session's prose is provider evidence only when the session did not claim
/// success — a session that exited 0 has asserted it is finished, and
/// VISION.md §3's invariant 4 makes the gates the answer to that, so a sentence
/// mentioning `429` in a successful run cannot impersonate a provider fault.
/// A [`FailureClass::NeedsInput`] is the exception: it is a claim about what the
/// agent is waiting for rather than a claim that it succeeded, so it is read
/// whatever the exit code was. Gate text is read by the two arms a gate is the
/// evidence for — policy and environment — and never by the provider arms,
/// because a gate that refused for a reason inside the repository is not the
/// provider's doing.
///
/// # The one error slot
///
/// `git_error` is the error the run failed with, not only a git one: T064's
/// signature has one slot for it, and ADR-0054 and ADR-0057 both record that
/// the slot cannot hold a provider refusal and an `Outcome` at once — a session
/// killed by a signal produces the refusal and no `Outcome` at all, so a caller
/// that has both is a caller under the `tdd` protocol's several sessions. Every
/// arm therefore matches on the variant rather than on what the caller meant:
/// an [`Error::Config`] is a configuration failure, an [`Error::Git`] a
/// conflict, an [`Error::Policy`] a policy failure, an [`Error::Provider`] a
/// provider failure, and an error the taxonomy names no class for
/// ([`Error::Io`], [`Error::Database`], [`Error::Serde`], [`Error::Corrupt`],
/// [`Error::NotFound`], [`Error::InvalidTransition`]) is the machine's, because
/// the fallback would otherwise blame an agent for its supervisor.
#[must_use]
pub fn classify(
    outcome: &Outcome,
    gates: &[GateResult],
    git_error: Option<&Error>,
) -> FailureClass {
    let provider = provider_text(outcome, git_error);
    let refused = refused_gate_text(gates);
    if is_configuration(git_error, &provider) {
        return FailureClass::ProviderConfiguration;
    }
    if limit_message(&provider, &[]).is_some() {
        return FailureClass::ProviderLimit;
    }
    if TRANSIENT.matches(&provider) {
        return FailureClass::ProviderTransient;
    }
    if is_git_conflict(git_error) {
        return FailureClass::GitConflict;
    }
    if is_policy(git_error, &refused) {
        return FailureClass::PolicyFailure;
    }
    if is_verification(gates) {
        return FailureClass::VerificationFailure;
    }
    if NEEDS_INPUT.matches(&session_text(outcome)) {
        return FailureClass::NeedsInput;
    }
    if is_environment(git_error, gates, &provider, &refused) {
        return FailureClass::EnvironmentFailure;
    }
    if is_provider_fault(git_error, &provider) {
        return FailureClass::ProviderTransient;
    }
    FailureClass::AgentFailure
}

/// Whether the fault is in what was configured, which no retry can fix.
fn is_configuration(error: Option<&Error>, provider: &str) -> bool {
    matches!(error, Some(Error::Config { .. })) || CONFIGURATION.matches(provider)
}

/// Whether a `git` refused the run, as opposed to a `git` that was not there.
fn is_git_conflict(error: Option<&Error>) -> bool {
    matches!(error, Some(Error::Git { stderr, .. }) if !GIT_UNSTARTED.matches(stderr))
}

/// Whether a rule was broken, by a refusal or by a gate that enforces one.
fn is_policy(error: Option<&Error>, refused: &str) -> bool {
    matches!(error, Some(Error::Policy { .. })) || POLICY.matches(refused)
}

/// Whether a check ran to a verdict and refused it.
fn is_verification(gates: &[GateResult]) -> bool {
    gates
        .iter()
        .any(|gate| !gate.passed && reached_a_verdict(gate))
}

/// Whether the machine, rather than the work, refused: a command that never
/// started, a host that cannot do what the gate asked, or a fault of the
/// supervisor itself.
///
/// The two halves of the gate test are not interchangeable. A gate records
/// `passed` only when its command exited successfully within its budget, so a
/// passing gate always carries a status and has therefore reached a verdict:
/// reading the pair as "failing *or* without a verdict" would let a gate that
/// passed argue that the machine refused, which is the one thing a green gate
/// cannot mean.
fn is_environment(
    error: Option<&Error>,
    gates: &[GateResult],
    provider: &str,
    refused: &str,
) -> bool {
    gates
        .iter()
        .any(|gate| !gate.passed && !reached_a_verdict(gate))
        || matches!(
            error,
            Some(
                Error::Io(_)
                    | Error::Database(_)
                    | Error::Serde(_)
                    | Error::NotFound { .. }
                    | Error::InvalidTransition { .. }
                    | Error::Corrupt { .. }
            )
        )
        || matches!(error, Some(Error::Git { stderr, .. }) if GIT_UNSTARTED.matches(stderr))
        || ENVIRONMENT.matches(&format!("{provider}{refused}"))
}

/// Whether the provider refused, in words or by the caller's own account.
fn is_provider_fault(error: Option<&Error>, provider: &str) -> bool {
    matches!(error, Some(Error::Provider { .. })) || PROVIDER_RESIDUAL.matches(provider)
}

/// Whether a gate stopped with an answer of its own to be believed.
///
/// A 126 or a 127 is not an answer: nothing was run, so nothing was checked.
/// Neither is a process killed by a signal that the runner did not send. A
/// timeout *is* an answer, because the budget the project configured is the
/// thing that was exceeded.
fn reached_a_verdict(gate: &GateResult) -> bool {
    gate.timed_out
        || matches!(gate.exit_code, Some(code) if code != NOT_EXECUTABLE && code != NOT_FOUND)
}

/// Everything a session printed, in the order a reader would read it.
///
/// Standard error first, because [`Outcome`] keeps the streams apart for exactly
/// this reading and a problem is what is being looked for.
fn session_text(outcome: &Outcome) -> String {
    format!("{}\n{}", outcome.stderr, outcome.stdout)
}

/// The evidence that a provider, rather than a gate or the work, is answering.
///
/// A session's prose joins the reading only when it exited non-zero: an exit 0
/// is a claim of success and the gates decide whether it is true. A
/// refusal handed in by the caller is read whatever the session exited with,
/// because the refusal is the fact and the session that preceded it may have
/// said nothing at all.
///
/// A provider refusal contributes its `detail`, and not the sentence this
/// project wraps around it. The wrapper is ours — `provider `claude` failed:
/// …` — so a table that matched it would recognise a refusal from the wrapper
/// rather than from anything the provider said, which would leave the variant
/// check in [`is_provider_fault`] as the only thing deciding the class while no
/// test could tell that check from a phrase. A configuration refusal
/// contributes no words at all, because its variant decides in the first arm:
/// text nothing can change an answer for is not evidence, it is a second path to
/// one class, and a classifier with two paths per class cannot be tested arm by
/// arm.
fn provider_text(outcome: &Outcome, error: Option<&Error>) -> String {
    let mut text = String::new();
    if outcome.exit_code != 0 {
        text.push_str(&session_text(outcome));
    }
    if let Some(Error::Provider { detail, .. }) = error {
        text.push('\n');
        text.push_str(detail);
    }
    text
}

/// What the gates that refused printed, in the order they ran.
///
/// A gate that passed has nothing to explain, and including its output would
/// let a passing run's incidental prose ("no untracked files", "permission
/// denied checks skipped") argue about a class it was never asked about.
fn refused_gate_text(gates: &[GateResult]) -> String {
    let mut text = String::new();
    for gate in gates.iter().filter(|gate| !gate.passed) {
        text.push_str(&gate.stdout);
        text.push('\n');
        text.push_str(&gate.stderr);
        text.push('\n');
    }
    text
}

/// The line on which a provider usage limit is named, if `text` holds one.
///
/// The whole trimmed line comes back, because the reset time is the useful half
/// of a limit and it is always on the same line as the words that say *limit*:
/// the task that waits a limit out parses this line rather than re-reading the
/// transcript. `patterns` are the ones an operator configured in addition to
/// the built-in table, which is how a provider whose wording no release has
/// learned yet is waited out today rather than next version.
///
/// Lines are read in order and each is matched against every pattern, so the
/// first limit named in the transcript is the answer whatever named it. A
/// configured pattern that is not a regular expression costs its own match and
/// nothing else — the refusal that keeps a bad pattern from being a silent hole
/// belongs to the configuration's own door, as it does for
/// [`crate::redact::check_patterns`].
#[must_use]
pub fn limit_message(text: &str, patterns: &[String]) -> Option<String> {
    let configured: Vec<Regex> = patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect();
    text.lines().find_map(|line| {
        let named = line.trim();
        let is_limit = !named.is_empty()
            && LIMIT
                .compiled()
                .iter()
                .chain(configured.iter())
                .any(|phrase| phrase.is_match(named));
        is_limit.then(|| named.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIGURATION, ENVIRONMENT, FailureClass, GIT_UNSTARTED, LIMIT, NEEDS_INPUT, POLICY,
        PROVIDER_RESIDUAL, TRANSIENT, TddException, classify, limit_message,
    };
    use crate::{Error, GateKind, GateResult, Outcome};
    use proptest::prelude::*;
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;
    use std::path::PathBuf;

    /// Every `FailureClass`, in the order `docs/DESIGN.md` declares them.
    const FAILURE_CLASSES: [FailureClass; 9] = [
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

    /// The same nine names as `docs/DESIGN.md` spells them, written a second
    /// time so a rename cannot pass by matching itself.
    const FAILURE_CLASS_NAMES: [&str; 9] = [
        "AgentFailure",
        "VerificationFailure",
        "ProviderLimit",
        "ProviderTransient",
        "ProviderConfiguration",
        "GitConflict",
        "EnvironmentFailure",
        "PolicyFailure",
        "NeedsInput",
    ];

    /// Every `TddException`, the four categories `VISION.md` allows.
    const TDD_EXCEPTIONS: [TddException; 4] = [
        TddException::Documentation,
        TddException::PureRefactoring,
        TddException::BuildConfiguration,
        TddException::ExistingFailingTest,
    ];

    /// The same four names, spelled out a second time.
    const TDD_EXCEPTION_NAMES: [&str; 4] = [
        "Documentation",
        "PureRefactoring",
        "BuildConfiguration",
        "ExistingFailingTest",
    ];

    /// Encodes `value`, insists it carries the documented name, reads it back.
    fn round_trips<T>(value: &T, name: &str)
    where
        T: Copy + serde::Serialize + DeserializeOwned + PartialEq + Debug,
    {
        let encoded = serde_json::to_string(value).expect("vocabulary encodes as JSON");
        assert_eq!(
            encoded,
            format!("\"{name}\""),
            "{name} must encode as its own name"
        );
        let decoded: T =
            serde_json::from_str(&encoded).expect("a documented encoding is read back");
        assert_eq!(&decoded, value, "{name} must survive the round trip");
    }

    #[test]
    fn failure_class_has_the_nine_variants_docs_design_md_names() {
        assert_eq!(FAILURE_CLASSES.len(), 9);
        assert_eq!(FAILURE_CLASS_NAMES.len(), FAILURE_CLASSES.len());
        for (class, name) in FAILURE_CLASSES.iter().zip(FAILURE_CLASS_NAMES) {
            round_trips(class, name);
        }
    }

    #[test]
    fn tdd_exception_has_the_four_categories_vision_md_allows() {
        assert_eq!(TDD_EXCEPTIONS.len(), 4);
        assert_eq!(TDD_EXCEPTION_NAMES.len(), TDD_EXCEPTIONS.len());
        for (exception, name) in TDD_EXCEPTIONS.iter().zip(TDD_EXCEPTION_NAMES) {
            round_trips(exception, name);
        }
    }

    #[test]
    fn the_vocabulary_refuses_a_name_no_document_names() {
        for rejected in ["RateLimited", "agent_failure", "Unknown", ""] {
            assert!(
                serde_json::from_str::<FailureClass>(&format!("\"{rejected}\"")).is_err(),
                "{rejected} is not a failure class and must not deserialise as one",
            );
        }
        for rejected in ["Style", "Documentation ", "documentation"] {
            assert!(
                serde_json::from_str::<TddException>(&format!("\"{rejected}\"")).is_err(),
                "{rejected} is not a tdd exception and must not deserialise as one",
            );
        }
    }

    /// A session that ran, printed what it printed, and exited with `exit_code`.
    fn session(exit_code: i32, stdout: &str, stderr: &str) -> Outcome {
        Outcome {
            exit_code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            usage: None,
            session_id: None,
            model_reported: None,
        }
    }

    /// A gate that ran to a verdict and refused it.
    fn gate_refused(kind: GateKind, exit_code: i32, stdout: &str) -> GateResult {
        GateResult {
            kind,
            passed: false,
            exit_code: Some(exit_code),
            signal: None,
            duration_ms: 4_213,
            stdout: stdout.to_owned(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    /// A gate that stopped without a verdict of its own.
    fn gate_without_a_verdict(
        kind: GateKind,
        exit_code: Option<i32>,
        signal: Option<i32>,
        stderr: &str,
    ) -> GateResult {
        GateResult {
            kind,
            passed: false,
            exit_code,
            signal,
            duration_ms: 91,
            stdout: String::new(),
            stderr: stderr.to_owned(),
            timed_out: false,
        }
    }

    /// The completion set of a run whose every gate passed.
    fn every_gate_passed() -> Vec<GateResult> {
        GateKind::ALL
            .iter()
            .map(|kind| GateResult {
                kind: *kind,
                passed: true,
                exit_code: Some(0),
                signal: None,
                duration_ms: 8_120,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
            .collect()
    }

    /// The verify gate refusing a test that failed, which is the evidence a
    /// remediation is written against.
    fn verify_refused() -> GateResult {
        gate_refused(
            GateKind::Verify,
            1,
            "test result: FAILED. 41 passed; 1 failed; 0 ignored; 8 measured\n\
             assert left == right failed: left `3f2a1c9`, right `91ab004`\n",
        )
    }

    /// What `git push` writes when the remote has moved under the candidate.
    fn rejected_push() -> Error {
        Error::Git {
            args: vec![
                "push".to_owned(),
                "origin".to_owned(),
                "--".to_owned(),
                "3f2a1c9:refs/heads/main".to_owned(),
            ],
            stderr: concat!(
                "To github.com:acme/invoice.git\n",
                " ! [rejected]        3f2a1c9 -> main (fetch first)\n",
                "error: failed to push some refs to 'github.com:acme/invoice.git'\n",
                "hint: Updates were rejected because the remote contains work that you do not\n",
            )
            .to_owned(),
        }
    }

    /// What `publish` writes when the tip a fetch brought back is not the
    /// candidate it was asked to prove.
    fn remote_without_the_candidate() -> Error {
        Error::Git {
            args: vec![
                "rev-parse".to_owned(),
                "--verify".to_owned(),
                "refs/remotes/origin/main".to_owned(),
            ],
            stderr: "after `git fetch origin` the tip of `origin/main` is 91ab004, which is not \
                     the candidate 3f2a1c9: the remote does not hold the commit this was asked \
                     to publish"
                .to_owned(),
        }
    }

    /// What `git.rs` writes when there is no `git` to start, in its own words.
    fn git_without_git() -> Error {
        Error::Git {
            args: vec!["status".to_owned(), "--porcelain".to_owned()],
            stderr: "`git` could not be started in `/home/etf/wt/task-7`: No such file or \
                     directory (os error 2)"
                .to_owned(),
        }
    }

    /// The refusal that says the tree was not the state verification needs.
    fn tree_was_dirty() -> Error {
        Error::Policy {
            detail: "uncommitted work at `/home/etf/wt/task-7`: modified: \
                     crates/ktask-core/src/gate.rs"
                .to_owned(),
            paths: vec![PathBuf::from("crates/ktask-core/src/gate.rs")],
        }
    }

    /// A refusal the provider layer handed back, in its own words.
    ///
    /// A caller can hold an [`Outcome`] and one of these at the same time: under
    /// the `tdd` protocol (VISION.md §9) one attempt runs several sessions, so a
    /// later session's refusal arrives beside an earlier session's report.
    fn provider_refused(detail: &str) -> Error {
        Error::Provider {
            provider: "claude".to_owned(),
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn a_credential_the_provider_refused_is_a_provider_configuration_failure() {
        let outcome = session(
            1,
            "",
            "API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\
             \"message\":\"invalid x-api-key\"}}",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderConfiguration,
            "a run that cannot authenticate pauses for a human; remediation cannot fix a key",
        );
    }

    #[test]
    fn a_model_the_provider_does_not_have_is_a_provider_configuration_failure() {
        let outcome = session(1, "", "ERROR: model \"gpt-7-astra\" does not exist");
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderConfiguration,
            "an invalid model is named by the class that says no retry can fix it",
        );
    }

    #[test]
    fn a_configuration_refusal_from_the_provider_layer_is_a_configuration_failure() {
        let error = Error::Config {
            key: "model".to_owned(),
            detail: "the session reported gpt-6-astra, which is not the configured \
                     gpt-6-astra:xhigh"
                .to_owned(),
        };
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::ProviderConfiguration,
            "ADR-0057's refusal is a configuration failure whatever the session exited with",
        );
    }

    #[test]
    fn a_command_that_cannot_be_executed_is_not_a_transient_failure() {
        let error = provider_refused(
            "the configured command `claude` is not an executable program on `PATH`: nothing \
             was started, and no retry can start one",
        );
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::ProviderConfiguration,
            "ADR-0054's missing CLI must not become a class that retries it",
        );
    }

    #[test]
    fn a_plan_limit_is_a_provider_limit_failure_and_not_an_agent_failure() {
        let outcome = session(
            1,
            "Reading the queue...",
            "You have hit your plan's usage limit. It resets at 2026-09-20T00:00:00Z",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderLimit,
            "a limit is waited out to the reset; an agent failure spends the remediation budget",
        );
    }

    #[test]
    fn a_limit_is_recognised_from_whichever_stream_the_cli_wrote_it_to() {
        let message = "ERROR: usage limit reached; retry after 14:05 UTC";
        let on_stdout = session(1, message, "");
        let on_stderr = session(1, "", message);
        assert_eq!(classify(&on_stdout, &[], None), FailureClass::ProviderLimit);
        assert_eq!(classify(&on_stderr, &[], None), FailureClass::ProviderLimit);
    }

    #[test]
    fn limit_message_reports_the_whole_line_the_limit_was_named_on() {
        let text = "reading the queue\nERROR: usage limit reached; resets at \
                    2026-09-20T00:00:00Z\ndone\n";
        assert_eq!(
            limit_message(text, &[]).as_deref(),
            Some("ERROR: usage limit reached; resets at 2026-09-20T00:00:00Z"),
            "T066 parses a reset out of the message, so the line is the answer",
        );
    }

    #[test]
    fn a_configured_pattern_recognises_a_limit_no_default_names() {
        let text = "allowance exhausted for this workspace";
        let patterns = vec!["allowance exhausted[ a-z]*".to_owned()];
        assert_eq!(limit_message(text, &patterns).as_deref(), Some(text));
        assert_eq!(limit_message(text, &[]), None);
    }

    #[test]
    fn text_that_names_no_limit_answers_with_nothing() {
        assert_eq!(limit_message("test result: ok. 42 passed\n", &[]), None);
    }

    #[test]
    fn a_configured_pattern_that_is_not_a_regex_is_skipped_and_not_fatal() {
        let patterns = vec!["(unclosed".to_owned()];
        assert_eq!(
            limit_message("rate limit exceeded", &patterns).as_deref(),
            Some("rate limit exceeded"),
            "a broken pattern costs its own match, not the whole reading",
        );
        assert_eq!(limit_message("nothing to read here", &patterns), None);
    }

    #[test]
    fn a_connection_that_died_on_the_way_to_the_provider_is_transient() {
        let outcome = session(
            1,
            "",
            "ERROR: stream disconnected before completion: read ECONNRESET",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderTransient,
            "the same work asked again is the whole recovery for this class",
        );
    }

    #[test]
    fn a_session_stopped_by_a_signal_is_a_provider_transient_failure() {
        let error = provider_refused(
            "was terminated by signal 9 before it reported an exit status; read 12 KiB of stdout",
        );
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::ProviderTransient,
        );
    }

    #[test]
    fn a_rejected_push_is_a_git_conflict() {
        let error = rejected_push();
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::GitConflict,
            "branch drift has a mechanical answer, so it is remediated and not paused on",
        );
    }

    #[test]
    fn a_publication_the_remote_did_not_take_is_a_git_conflict() {
        let error = remote_without_the_candidate();
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::GitConflict
        );
    }

    #[test]
    fn a_git_that_could_not_be_started_is_an_environment_failure() {
        let error = git_without_git();
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::EnvironmentFailure,
            "no repository decision was made: the host has no git to make one with",
        );
    }

    #[test]
    fn a_tree_that_was_dirty_at_verification_time_is_a_policy_failure() {
        let error = tree_was_dirty();
        assert_eq!(
            classify(&session(0, "", ""), &[verify_refused()], Some(&error)),
            FailureClass::PolicyFailure,
            "the rule broken is the finding; the red gate behind it is what the dirty tree did",
        );
    }

    #[test]
    fn a_gate_that_names_a_forbidden_path_is_a_policy_failure() {
        let gate = gate_refused(
            GateKind::Privacy,
            1,
            "forbidden path .ktask/context.md is tracked in the repository\n",
        );
        assert_eq!(
            classify(&session(1, "", ""), &[gate], None),
            FailureClass::PolicyFailure,
        );
    }

    #[test]
    fn a_failing_test_is_a_verification_failure() {
        assert_eq!(
            classify(
                &session(0, "the suite is green for me", ""),
                &[verify_refused()],
                None
            ),
            FailureClass::VerificationFailure,
        );
    }

    #[test]
    fn a_gate_that_ran_out_of_its_budget_is_a_verification_failure() {
        let mut budget = gate_without_a_verdict(GateKind::Build, None, Some(15), "");
        budget.timed_out = true;
        assert_eq!(
            classify(&session(0, "", ""), &[budget], None),
            FailureClass::VerificationFailure,
            "a gate out of the budget this project configured is the project's to fix",
        );
    }

    #[test]
    fn a_gate_whose_command_could_not_be_executed_is_an_environment_failure() {
        let not_executable = gate_without_a_verdict(GateKind::Build, Some(126), None, "");
        assert_eq!(
            classify(&session(0, "", ""), &[not_executable], None),
            FailureClass::EnvironmentFailure,
            "nothing was compiled and no test ran, so no check can be said to have failed",
        );
        let not_found = gate_without_a_verdict(
            GateKind::Verify,
            Some(127),
            None,
            "sh: cargo: command not found\n",
        );
        assert_eq!(
            classify(&session(0, "", ""), &[not_found], None),
            FailureClass::EnvironmentFailure,
        );
    }

    #[test]
    fn a_gate_killed_by_a_signal_nothing_here_sent_is_an_environment_failure() {
        let killed = gate_without_a_verdict(GateKind::Verify, None, Some(9), "");
        assert_eq!(
            classify(&session(0, "", ""), &[killed], None),
            FailureClass::EnvironmentFailure,
            "an outside kill is the machine, not the work",
        );
    }

    #[test]
    fn a_host_that_refused_the_filesystem_is_an_environment_failure() {
        let outcome = session(1, "", "failed to write the report: Read-only file system");
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::EnvironmentFailure,
        );
    }

    #[test]
    fn a_fault_of_the_supervisor_is_never_blamed_on_the_agent() {
        let error = Error::Corrupt {
            detail: "the payload of this record is not the JSON it claims to hold".to_owned(),
            seq: Some(7),
        };
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::EnvironmentFailure,
            "the taxonomy names no class for a journal that could not be read, and the \
             fallback is not where an innocent agent takes the blame for it",
        );
    }

    #[test]
    fn a_session_that_reports_needs_input_pauses_for_a_decision() {
        let outcome = session(
            1,
            "KTASK_RESULT: NEEDS_INPUT\nSummary: the export format is unresolved.\n",
            "",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::NeedsInput,
            "an unmade decision pauses the queue for the human who owns it",
        );
    }

    #[test]
    fn a_report_line_quoted_inside_a_sentence_is_not_a_request_for_input() {
        let outcome = session(
            1,
            "The template says a line beginning `KTASK_RESULT: NEEDS_INPUT` is only for \
             genuine ambiguity, and this is not one.\n",
            "",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::AgentFailure,
            "a prompt echoed back into a session is not the agent asking a question",
        );
    }

    #[test]
    fn a_provider_fault_in_words_no_table_names_is_still_a_provider_failure() {
        let outcome = session(
            1,
            "",
            "API Error: 599 <upstream answered something unreadable>",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderTransient,
            "an unrecognised fault of the provider is retried within bounds, never spent as \
             an agent failure",
        );
    }

    #[test]
    fn a_provider_whose_only_complaint_is_that_it_failed_is_not_blamed_on_the_work() {
        let outcome = session(
            1,
            "",
            "provider reported a failure while streaming the answer",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderTransient,
            "the residual arm reads a refusal that names who refused beside a word that says \
             it failed, which is the shape of a complaint no phrase above names",
        );
    }

    #[test]
    fn an_unrecognised_provider_refusal_is_never_absorbed_by_the_fallback() {
        let error = provider_refused("the session answered something this adapter cannot read");
        assert_eq!(
            classify(&session(0, "", ""), &[], Some(&error)),
            FailureClass::ProviderTransient,
            "the fallback says the work failed; a refusal of the provider is not that",
        );
    }

    #[test]
    fn a_session_that_failed_at_its_own_work_is_the_fallback_and_nothing_else() {
        assert_eq!(
            classify(&session(1, "", ""), &[], None),
            FailureClass::AgentFailure
        );
        let explained = session(
            1,
            "I could not get the migration to apply and stopped.\n",
            "",
        );
        assert_eq!(
            classify(&explained, &[], None),
            FailureClass::AgentFailure,
            "a session that ran, refused, and named no other reason failed at the work",
        );
    }

    #[test]
    fn configuration_is_read_before_the_limit_reported_beside_it() {
        let outcome = session(
            1,
            "",
            "usage limit reached for this plan; please log in again once the plan is raised",
        );
        assert_eq!(
            classify(&outcome, &[], None),
            FailureClass::ProviderConfiguration,
        );
    }

    #[test]
    fn a_provider_fault_is_read_before_the_gate_that_failed_alongside_it() {
        let outcome = session(1, "", "You have hit your plan's usage limit.");
        assert_eq!(
            classify(&outcome, &[verify_refused()], None),
            FailureClass::ProviderLimit,
        );
    }

    #[test]
    fn a_provider_fault_is_read_before_the_git_refusal_behind_it() {
        let outcome = session(
            1,
            "",
            "stream disconnected before completion: unexpected EOF during chunk size line",
        );
        let error = rejected_push();
        assert_eq!(
            classify(&outcome, &[], Some(&error)),
            FailureClass::ProviderTransient
        );
    }

    #[test]
    fn a_failing_gate_is_read_before_a_decision_the_same_session_asked_for() {
        let outcome = session(1, "KTASK_RESULT: NEEDS_INPUT\n", "");
        assert_eq!(
            classify(&outcome, &[verify_refused()], None),
            FailureClass::VerificationFailure,
            "the ordered chain the task fixes puts verification first; a decision arrives as \
             a DecisionRaised event rather than as text this table has to guess at",
        );
    }

    #[test]
    fn a_session_that_claimed_success_is_answered_by_its_gates_not_its_prose() {
        let outcome = session(
            0,
            "The earlier 429 too many requests was retried; the work is complete.\n",
            "",
        );
        assert_eq!(
            classify(&outcome, &[verify_refused()], None),
            FailureClass::VerificationFailure,
            "a session that exited 0 has claimed success, and invariant 4 says the gates \
             decide whether that is true — its prose cannot impersonate a provider fault",
        );
    }

    /// Every phrase in every table is a regular expression.
    ///
    /// A phrase that does not compile is skipped when the table is built, which
    /// would otherwise be a rule that quietly stopped existing: the class it
    /// named would keep being chosen for everything else that reaches it, and
    /// only a misclassified run would notice.
    #[test]
    fn every_table_compiles() {
        for (name, table) in [
            ("configuration", &CONFIGURATION),
            ("limit", &LIMIT),
            ("transient", &TRANSIENT),
            ("policy", &POLICY),
            ("environment", &ENVIRONMENT),
            ("provider residual", &PROVIDER_RESIDUAL),
            ("git unstarted", &GIT_UNSTARTED),
            ("needs input", &NEEDS_INPUT),
        ] {
            assert_eq!(
                table.compiled().len(),
                table.phrases.len(),
                "{name} holds a phrase that is not a regular expression",
            );
        }
    }

    proptest! {
        /// The class is decided by the evidence in front of it and by nothing
        /// else: no clock, no randomness, no memory of a previous call.
        #[test]
        fn classification_is_the_same_answer_for_the_same_evidence(
            exit_code in any::<i32>(),
            stdout in "[a-z0-9 ]{0,80}",
            stderr in "[a-z0-9 ]{0,80}",
        ) {
            let outcome = session(exit_code, &stdout, &stderr);
            let gates = [verify_refused()];
            let first = classify(&outcome, &gates, None);
            prop_assert_eq!(first, classify(&outcome, &gates, None));
        }

        /// A run with no gates and no error can be faulted only by its provider,
        /// by the machine it is standing on, or by the work itself: a class that
        /// needs evidence nobody supplied is never invented out of session text.
        #[test]
        fn a_class_that_needs_evidence_is_never_invented_from_a_session_alone(
            exit_code in any::<i32>(),
            text in "[a-z0-9 ]{0,120}",
        ) {
            let outcome = session(exit_code, &text, &text);
            let class = classify(&outcome, &[], None);
            prop_assert!(
                matches!(
                    class,
                    FailureClass::AgentFailure
                        | FailureClass::ProviderConfiguration
                        | FailureClass::ProviderLimit
                        | FailureClass::ProviderTransient
                        | FailureClass::EnvironmentFailure
                ),
                "invented {class:?} out of {text:?}",
            );
        }

        /// With every gate green and the session reporting success, only the work
        /// itself is left to answer for the failure.
        #[test]
        fn a_run_with_nothing_to_fault_but_the_work_falls_back_to_the_work(
            stdout in "[a-z0-9]{0,80}",
        ) {
            let outcome = session(0, &stdout, "");
            prop_assert_eq!(
                classify(&outcome, &every_gate_passed(), None),
                FailureClass::AgentFailure,
            );
        }
    }
}

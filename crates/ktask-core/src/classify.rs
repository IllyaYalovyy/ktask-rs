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
use time::macros::format_description;
use time::{Date, Duration, OffsetDateTime, PlainDateTime, Time, UtcOffset};

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

/// The words a plan limit is named with, before an operator configures more.
///
/// Every phrase is wording a provider writes rather than a paraphrase of one,
/// and `LIMIT_FIXTURES` in the tests holds the line each was written for. That
/// upkeep is worth paying because a limit is the class reached only by reading
/// text: miss the wording and the same evidence lands on the fallback, which
/// spends a bounded remediation on what should have been a pause and reports
/// exit 1 where `docs/CONTRACT.md` §1 says a parked queue exits 3.
///
/// Both CLIs say it twice over — a sentence for whoever is watching the run, and
/// a machine-readable error `type` for the log — so the table reads both. A JSON
/// body whose whole content is `"type":"rate_limit_error"` is as much a limit as
/// the subscription sentence Claude Code prints above it, and Codex's quota
/// refusal arrives as prose and as `insufficient_quota` in the same release.
///
/// What is deliberately *not* here is as much of the table as what is.
/// `overloaded_error` and a dropped stream belong to
/// [`TRANSIENT`]: there is no plan ceiling to wait out, and the same work asked
/// again usually lands. A context-window refusal ("prompt is too long") is
/// named nowhere either, even though it is a limit of a kind — it has no reset
/// time, so reading it as [`FailureClass::ProviderLimit`] would back off from a
/// request that will be refused identically forever, where the class the run
/// lands in should be the one a shorter prompt gets out of. An operator whose
/// provider words a limit differently configures it rather than waiting for a
/// release: `patterns` of [`limit_message`] exists for exactly that.
static LIMIT: Table = Table::new(&[
    // The ceiling, named by what it is a ceiling on. Claude Code leads with the
    // window (`session`, `daily`, `weekly`); an Anthropic organization that has
    // spent its budget says `spend limit` and means the month.
    concat!(
        r"(?i)\b(?:usage|rate|quota|credit|plan|billing|request|subscription|session|daily",
        r"|weekly|monthly|spend)[ _-]?limits?\b",
    ),
    // The ceiling first and what happened to it second, which is how OpenAI's
    // tokens-per-minute refusal is written: the limit noun comes before the
    // number that was crossed.
    r"(?i)\blimits?\b[^\n]{0,24}\b(?:reached|exceeded|hit|applied)\b",
    // What ran out, said before the thing that ran out: OpenAI's quota refusal
    // leads with the verb, and `credits`/`allowance` are how a plan is named
    // once there is no limit noun in the sentence at all.
    r"(?i)\b(?:hit|reached|exceeded|over)\b[^\n]{0,24}\b(?:limits?|quota|credits?|allowance)\b",
    // The status both APIs answer with, and the reason phrase that is the whole
    // body when a CLI prints the HTTP line instead of the JSON behind it.
    r"(?i)\b429\b",
    r"(?i)too many requests",
    // The money door, which both providers end at: a quota a platform team set,
    // a balance Anthropic says is "too low", credits that have run out.
    r"(?i)\bquota\b[^\n]{0,24}\b(?:exhausted|exceeded|reached|depleted)\b",
    r"(?i)insufficient[^\n]{0,24}\b(?:quota|credits?|balance|funds)\b",
    r"(?i)credit balance[^\n]{0,24}\b(?:too low|exhausted|depleted|empty)\b",
    r"(?i)\b(?:out of|depleted|exhausted|used up)\b[^\n]{0,12}\bcredits?\b",
    // How long to wait, which both APIs send: the HTTP header and the prose
    // form are one rule, and the reset time sits beside it on the same line.
    r"(?i)\bretry[ _-]?after\b",
    // The error `type`s, which are what a JSON error body carries when it
    // carries no prose to read at all.
    r"(?i)\b(?:rate|usage|plan|quota|credit|billing)[ _-]?limit[ _-]?(?:error|exceeded)\b",
    r"(?i)\binsufficient[ _-]?quota\b",
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

/// What a limit costs the run: an instant to wake at, or a bounded interval.
///
/// VISION.md §7 gives a [`FailureClass::ProviderLimit`] two responses and no
/// third: a reset the provider named is waited out to that exact instant, and a
/// limit that named no reset backs off within bounds. [`wait_plan`] chooses
/// between them once, from the instant [`parse_reset`] read and the two
/// configured ceilings, so whatever sleeps is nowhere near the decision and
/// cannot invent a third response to a limit.
///
/// Jitter is deliberately absent, although VISION.md §7 asks for it beside the
/// margin. This is a pure function of its arguments, and the wait must stay pure
/// to survive a restart: the journal holds the instant (ADR-0009), so a
/// supervisor that woke at an instant other than the one it wrote down would be
/// resuming into a wait longer or shorter than the one it promised. Whoever
/// sleeps adds jitter *around* the plan, never inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPlan {
    /// Wait until `at`, then ask the provider again: it said when.
    Deadline {
        /// The instant to wake at — the reset that was read, plus the margin.
        at: OffsetDateTime,
    },
    /// Wait `wait`, then ask the provider again, with no instant to aim at.
    Backoff {
        /// How long to sit: never negative, and never longer than the ceiling
        /// this plan was built under.
        wait: Duration,
    },
}

/// A shape a reset time is written with, compiled the first time it is read.
///
/// As with [`Table`], a shape that does not compile is skipped rather than
/// panicking, and `every_reset_pattern_compiles` is what stops that being a rule
/// that quietly stopped existing: every limit written in that shape would fall
/// back to a bounded wait with no instant in it, and only a run that waited the
/// wrong length would notice.
struct ResetPattern {
    /// The shape as it is written in this file.
    source: &'static str,
    /// The same shape compiled, on first use.
    compiled: OnceLock<Option<Regex>>,
}

impl ResetPattern {
    /// A shape that is not compiled yet.
    const fn new(source: &'static str) -> Self {
        Self {
            source,
            compiled: OnceLock::new(),
        }
    }

    /// The compiled shape, or `None` when the shape is not a regular expression.
    fn regex(&self) -> Option<&Regex> {
        self.compiled
            .get_or_init(|| Regex::new(self.source).ok())
            .as_ref()
    }
}

/// A day, with the time and the offset that may stand beside it.
///
/// The separator accepts `T`, one space, and the word `at`, which are the three
/// ways a reset is written down — as a machine-readable instant, as the same
/// instant typed by a human, and in a sentence.
static RESET_DAY: ResetPattern = ResetPattern::new(concat!(
    r"(?i)\b(\d{4}-\d{2}-\d{2})(?:(?:[T ]| ?at )(\d{2}:\d{2}(?::\d{2})?(?:\.\d{1,9})?)",
    r"(?: ?(Z|[+-]\d{2}:\d{2}))?)?\b",
));

/// A clock time, and the word or the zone that makes it a deadline.
///
/// Either the keyword in front (`at`, `by`, `until`, `till`) or the zone on the
/// tail (`UTC`, `GMT`, `Z`) is required, and it is required because a line of
/// output is full of clock times that say only when a line was printed. Which
/// day the clock falls on is not in the token: it is the next day it occurs on,
/// counted from the instant the line was read.
static RESET_CLOCK: ResetPattern = ResetPattern::new(
    r"(?i)\b(?:(at|by|until|till)[ \t]+)?(\d{1,2}:\d{2}(?::\d{2})?)(?:[ \t]*(utc|gmt|z))?\b",
);

/// A number beside the unit that says how long a wait it is.
///
/// The units are ordered longest first so `250ms` is read as a quarter of a
/// second rather than as `250m` with an `s` left over, and a fraction is kept
/// because the per-minute refusals name sub-second waits.
static RESET_SPAN: ResetPattern = ResetPattern::new(concat!(
    r"(?i)\b(\d+(?:\.\d+)?)[ \t]*(milliseconds?|ms|microseconds?|us|nanoseconds?|ns|weeks?|",
    r"wks?|w|days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|sec|s)\b",
));

/// The HTTP header's bare number of seconds, which carries no unit at all.
///
/// `retry-after` is the one form a provider sends a wait as a plain integer, and
/// the built-in limit table already recognises the header precisely because the
/// header is the answer — see ADR-0060. The words it may cross are bounded so a
/// `retry after` early in a long line cannot reach a number two clauses away.
static RESET_RETRY_AFTER: ResetPattern =
    ResetPattern::new(r"(?i)\bretry[ _-]?after\b[^\d\n]{0,8}(\d+(?:\.\d+)?)\b");

/// When a provider says the limit lifts, read out of the line that said it.
///
/// [`limit_message`] hands over a line; this reads the half of it that decides
/// the wait. Four shapes are read, in this order, and the first the line holds
/// is the answer whatever else the line says:
///
/// - **A day, with or without a time and an offset.** `2026-09-20T00:00:00Z`,
///   `2026-09-20 at 09:00` and a bare `2026-09-20` are all read. A time written
///   with no offset, and a day written with no time, mean UTC; a day with no
///   time means its first instant, which is the boundary a daily window lifts on.
/// - **A clock time** — `14:05`, `14:05:30`, with or without `UTC`, `GMT`, `Z` —
///   resolved to the next day it falls on, so midnight read at 23:59 is a minute
///   away rather than a day gone. It is only read when the line has made a
///   deadline of it, and not at all when more words follow that would place it
///   (`pm`, a named zone, `tomorrow`), because this parser has no twelve-hour
///   clock, no tzdata, and no rule for whose Tuesday anything is.
/// - **A duration** — `in 1.8s`, `2h 15m`, `3 days` — summed when written in
///   several parts, and measured from `now` rather than from midnight.
/// - **A bare `retry-after`**, whose number is seconds and is the whole answer.
///
/// A day that does not exist (`2026-13-45`) answers [`None`] rather than being
/// re-read as the clock time or the duration inside it: a line that names a day
/// no calendar holds has named no reset, and honouring half of it would be
/// waiting on a promise the provider never made. `None` is not a failure —
/// [`wait_plan`] turns it into a bounded backoff, which is the right response to
/// a wait nobody has been able to size.
///
/// `now` is not decoration. It decides which day a clock time falls on and where
/// every duration is measured from, and it is the caller's clock rather than one
/// read here, so the reading stays pure and a test can hold still.
#[must_use]
pub fn parse_reset(text: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    if let Some(day) = RESET_DAY.regex()
        && let Some(found) = day.captures(text)
    {
        return read_day(&found);
    }
    if let Some(clock) = RESET_CLOCK.regex()
        && let Some(instant) = clock
            .captures_iter(text)
            .find_map(|found| read_clock(text, &found, now))
    {
        return Some(instant);
    }
    read_span(text, now).or_else(|| read_retry_after(text, now))
}

/// How long to wait on a limit, given what was read from it and the two ceilings.
///
/// A known reset is waited out to the instant plus `margin`: that is VISION.md
/// §7's "waits until the exact reset time, with margin", and
/// [`crate::Config::limit_wait_margin_secs`] is the configuration's name for it.
/// The cushion is what stops a limit that lifts a second early from costing a
/// second refusal.
///
/// Everything else is a bounded backoff of the same configured pause, clamped
/// into `0 ..= max`, and the bound is the point. It covers a reset that was never
/// read, a reset whose instant has already passed (the promise is stale, and
/// waking *at* it means waking now to ask again immediately), and a reset beyond
/// [`crate::Config::limit_max_wait_secs`] — the ceiling a run will sit through
/// before it reports a limit rather than appearing to hang. A caller that must
/// tell "no reset" from "a reset too far out" still holds the `reset` it passed
/// in; what it can rely on from here is that no plan is negative and none runs
/// past `max`, so it can sleep without a watchdog of its own.
#[must_use]
pub fn wait_plan(
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
    margin: Duration,
    max: Duration,
) -> WaitPlan {
    let ceiling = max.max(Duration::ZERO);
    if let Some(instant) = reset
        && let Some(deadline) = instant.checked_add(margin)
        && deadline > now
        && deadline - now <= ceiling
    {
        return WaitPlan::Deadline { at: deadline };
    }
    WaitPlan::Backoff {
        wait: margin.max(Duration::ZERO).min(ceiling),
    }
}

/// The instant a day-shaped token names, or `None` when it names none.
fn read_day(found: &regex::Captures<'_>) -> Option<OffsetDateTime> {
    let format = format_description!("[year]-[month]-[day]");
    let day = Date::parse(found.get(1)?.as_str(), &format).ok()?;
    let Some(clock) = found.get(2) else {
        return Some(day.with_time(Time::MIDNIGHT).assume_utc());
    };
    let time = clock_of(clock.as_str())?;
    let offset = match found.get(3) {
        Some(mark) => offset_of(mark.as_str())?,
        None => UtcOffset::UTC,
    };
    Some(PlainDateTime::new(day, time).assume_offset(offset))
}

/// The next instant a clock-shaped token falls on, counted from `now`.
fn read_clock(
    text: &str,
    found: &regex::Captures<'_>,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    let made_a_deadline = found.get(1).is_some() || found.get(3).is_some();
    let tail = text.get(found.get(0)?.end()..).unwrap_or("");
    if !made_a_deadline || wants_more_words(tail) {
        return None;
    }
    let time = clock_of(found.get(2)?.as_str())?;
    let today = now.to_offset(UtcOffset::UTC).date();
    let moment = today.with_time(time).assume_utc();
    if moment >= now {
        return Some(moment);
    }
    Some(today.next_day()?.with_time(time).assume_utc())
}

/// The instant a run of `N unit` parts names, measured from `now`.
///
/// Only the first run is read: a line that names two waits ("try again in 20s;
/// the window resets in 4h") has one answer, which is the first one, and summing
/// the two would invent a deadline nobody sent.
fn read_span(text: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let finder = RESET_SPAN.regex()?;
    let mut total = 0.0_f64;
    let mut parts = 0;
    let mut end = 0;
    for found in finder.captures_iter(text) {
        let whole = found.get(0)?;
        let gap = text.get(end..whole.start()).unwrap_or("");
        if parts > 0 && !is_continuation(gap) {
            break;
        }
        total += part_seconds(&found)?;
        end = whole.end();
        parts += 1;
    }
    if parts == 0 {
        return None;
    }
    now.checked_add(Duration::saturating_seconds_f64(total))
}

/// The instant a `retry-after` header counts to, measured from `now`.
fn read_retry_after(text: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let found = RESET_RETRY_AFTER.regex()?.captures(text)?;
    let seconds = found.get(1)?.as_str().parse::<f64>().ok()?;
    now.checked_add(Duration::saturating_seconds_f64(seconds))
}

/// The `HH:MM[:SS]` a token was written with, to the second.
fn clock_of(token: &str) -> Option<Time> {
    let mut parts = token.split(':');
    let hour = parts.next()?.parse::<u8>().ok()?;
    let minute = parts.next()?.parse::<u8>().ok()?;
    let second = match parts.next() {
        Some(seconds) => seconds.split('.').next()?.parse::<u8>().ok()?,
        None => 0,
    };
    Time::from_hms(hour, minute, second).ok()
}

/// The offset an instant was written in, where `Z` means UTC.
fn offset_of(token: &str) -> Option<UtcOffset> {
    if token.eq_ignore_ascii_case("z") {
        return Some(UtcOffset::UTC);
    }
    let (sign, body) = match token.chars().next() {
        Some('+') => (1, token.get(1..)?),
        Some('-') => (-1, token.get(1..)?),
        _ => return None,
    };
    let (hours, minutes) = body.split_once(':')?;
    let seconds = hours.parse::<i32>().ok()? * 3_600 + minutes.parse::<i32>().ok()? * 60;
    UtcOffset::from_whole_seconds(sign * seconds).ok()
}

/// Whether the words between two `N unit` parts continue one duration.
fn is_continuation(gap: &str) -> bool {
    let gap = gap.trim();
    gap.is_empty()
        || gap.eq_ignore_ascii_case("and")
        || gap.chars().all(|c| matches!(c, ',' | '-' | ' ' | '\t'))
}

/// Whether more words follow a clock time — words that would place it.
///
/// A clock followed by `pm`, by a zone name, or by `tomorrow` is a deadline this
/// parser cannot honour. Reading `14:05 tomorrow` as `14:05 today` waits a whole
/// day early, which is the one answer worse than no answer: the token is refused
/// and the limit falls back to a bounded backoff.
fn wants_more_words(tail: &str) -> bool {
    tail.trim_start_matches([' ', '\t'])
        .chars()
        .next()
        .is_some_and(char::is_alphabetic)
}

/// How many seconds one `N unit` part is worth.
fn part_seconds(found: &regex::Captures<'_>) -> Option<f64> {
    let amount = found.get(1)?.as_str().parse::<f64>().ok()?;
    let unit = found.get(2)?.as_str().to_ascii_lowercase();
    Some(amount * seconds_per_unit(&unit)?)
}

/// The length of one unit, or `None` when the word is not a unit of time.
fn seconds_per_unit(unit: &str) -> Option<f64> {
    let seconds = match unit {
        "ns" | "nanosecond" | "nanoseconds" => 1e-9,
        "us" | "microsecond" | "microseconds" => 1e-6,
        "ms" | "millisecond" | "milliseconds" => 1e-3,
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "wk" | "wks" | "week" | "weeks" => 604_800.0,
        _ => return None,
    };
    Some(seconds)
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIGURATION, ENVIRONMENT, FailureClass, GIT_UNSTARTED, LIMIT, NEEDS_INPUT, POLICY,
        PROVIDER_RESIDUAL, RESET_CLOCK, RESET_DAY, RESET_RETRY_AFTER, RESET_SPAN, TRANSIENT,
        TddException, classify, limit_message,
    };
    // The three wait items are taken from the crate root rather than from this
    // module: a test that reaches the module directly would keep passing after
    // the re-export at `crate::` was dropped, and the kernel only ever sees the
    // root.
    use crate::{Error, GateKind, GateResult, Outcome, WaitPlan, parse_reset, wait_plan};
    use proptest::prelude::*;
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;
    use std::path::PathBuf;
    use time::macros::datetime;
    use time::{Duration, OffsetDateTime};

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

    /// The distance a phrase allows is part of the phrase.
    ///
    /// Both verb phrases reach across a few words at most, because a CLI's
    /// refusal is one clause long. Without that bound a paragraph about a
    /// budget and, two sentences later, an unrelated `limit` would be read as
    /// one refusal — and the run would pause on prose that named nothing.
    #[test]
    fn a_limit_noun_too_far_from_its_verb_is_not_a_limit() {
        let line = "You exceeded the budget the task was given for the whole run, and \
                    no limit was named anywhere in it.";
        assert_eq!(limit_message(line, &[]), None);
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

    /// One provider line for each built-in limit pattern, in table order.
    ///
    /// The wordings are the two CLIs this project drives rather than invented
    /// prose: Claude Code's subscription refusals and Anthropic's API error
    /// body for the first half, Codex's rate-limit snapshot and its
    /// `insufficient_quota` body for the second.
    ///
    /// The list is positional, so a pattern added to [`LIMIT`] without a line
    /// beside it fails the build. That is the point: `every_table_compiles`
    /// proves a default parses, and nothing else proves a default is the words
    /// a real provider writes — an unexercised default is a rule nobody has
    /// seen work, and a default narrowed by mistake stops recognising the
    /// release it was written for in silence.
    const LIMIT_FIXTURES: &[&str] = &[
        "You've hit your weekly limit; it will reset at 8pm (America/Los_Angeles) on Tuesday.",
        "Rate limit reached for gpt-5.1-codex in organization org-ktask on tokens per min \
         (TPM): Limit 30000, Used 29998, Requested 900. Please try again in 1.8s.",
        "You exceeded your current quota, please check your plan and billing details.",
        "HTTP 429 from api.anthropic.com",
        "Reason phrase: Too Many Requests",
        "Project quota exhausted; ask your organization to raise it.",
        "Insufficient credits to run this request; add a payment method to continue.",
        "API error (400): credit balance is too low",
        "You're out of credits; add more to keep the run going.",
        "retry-after: 3600",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded."}}"#,
        r#"{"error":{"type":"insufficient_quota","message":"You exceeded your current quota."}}"#,
    ];

    #[test]
    fn every_default_limit_pattern_has_a_fixture_the_defaults_recognise() {
        assert_eq!(
            LIMIT_FIXTURES.len(),
            LIMIT.phrases.len(),
            "a default limit pattern was added or removed without a fixture beside it",
        );
        for (index, &line) in LIMIT_FIXTURES.iter().enumerate() {
            let phrase = LIMIT.phrases[index];
            assert!(
                LIMIT.compiled()[index].is_match(line),
                "fixture {line:?} is no longer the line {phrase:?} was written for",
            );
            assert_eq!(
                limit_message(line, &[]).as_deref(),
                Some(line),
                "the defaults no longer recognise {line:?}, which {phrase:?} exists to catch",
            );
        }
    }

    #[test]
    fn a_default_limit_fixture_is_never_read_as_the_works_own_failure() {
        for &line in LIMIT_FIXTURES {
            let on_stdout = classify(&session(1, line, ""), &[], None);
            let on_stderr = classify(&session(1, "", line), &[], None);
            let beside_a_refused_gate = classify(&session(1, "", line), &[verify_refused()], None);
            assert_eq!(
                on_stdout,
                FailureClass::ProviderLimit,
                "{line:?} on stdout is waited out, not remediated",
            );
            assert_eq!(
                on_stderr,
                FailureClass::ProviderLimit,
                "{line:?} on stderr is waited out, not remediated",
            );
            assert_eq!(
                beside_a_refused_gate,
                FailureClass::ProviderLimit,
                "{line:?} is a limit even with a red gate beside it: the limit arm runs \
                 first, so a wait never spends a remediation on a test that failed \
                 because the session stopped mid-run",
            );
        }
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

    /// The morning a reset line is read against: early enough that most of the
    /// day's clock times are still ahead of it.
    fn morning() -> OffsetDateTime {
        datetime!(2026-09-19 09:00:00 UTC)
    }

    /// The last half-minute of a day, which is where a reset read as midnight
    /// has to land on the far side of rather than behind.
    fn last_minute_of_the_day() -> OffsetDateTime {
        datetime!(2026-09-19 23:59:30 UTC)
    }

    /// The two configured ceilings a limit is waited out under, as
    /// `docs/DESIGN.md` fixes them: a minute of cushion, a day of ceiling.
    const MARGIN: Duration = Duration::seconds(60);
    const CEILING: Duration = Duration::seconds(86_400);

    #[test]
    fn an_absolute_instant_is_read_from_the_line_that_named_the_limit() {
        let line = "You have hit your plan's usage limit. It resets at 2026-09-20T00:00:00Z";
        assert_eq!(
            parse_reset(line, morning()),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
            "VISION.md §7 waits a known reset out to the exact instant, so the \
             instant on the line is the answer the wait is built from",
        );
    }

    #[test]
    fn an_absolute_instant_keeps_the_offset_it_was_written_in() {
        let line = "usage limit reached; resets at 2026-09-20T09:00:00+09:00";
        assert_eq!(
            parse_reset(line, morning()),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
            "a reset written in the provider's own offset is the same instant as \
             the one waited out, so the offset cannot be dropped or assumed UTC",
        );
    }

    #[test]
    fn an_offset_behind_utc_is_subtracted_on_the_right_side() {
        assert_eq!(
            parse_reset(
                "usage limit reached; resets at 2026-09-20T02:00:00-05:30",
                morning(),
            ),
            Some(datetime!(2026-09-20 07:30:00 UTC)),
            "the sign decides which way both parts of the offset go, and an hour \
             and a minute that are each dropped or each added wake the run hours \
             either side of the instant the provider named",
        );
    }

    #[test]
    fn an_instant_written_without_an_offset_is_read_as_utc() {
        let line = "usage limit reached; resets 2026-09-20 09:00";
        assert_eq!(
            parse_reset(line, morning()),
            Some(datetime!(2026-09-20 09:00:00 UTC)),
            "a time with no zone is read as UTC rather than as the supervisor's \
             local time, which differs from machine to machine",
        );
    }

    #[test]
    fn fractional_seconds_on_an_instant_are_honoured_to_the_second() {
        assert_eq!(
            parse_reset("resets at 2026-09-20T00:00:00.500Z", morning()),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
            "a machine-written instant may carry a fraction the wait cannot use; \
             the margin beside it is a minute, so the fraction is truncated and \
             not carried into the plan",
        );
    }

    /// A day named without a time is that day's first instant, which is the
    /// day boundary the task's done-when asks for: read one minute before it,
    /// the wait is a minute long, and read one minute after it the promise has
    /// already been missed.
    #[test]
    fn a_day_written_without_a_time_resets_at_its_first_instant() {
        assert_eq!(
            parse_reset("usage limit reached; resets 2026-09-20", morning()),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
        );
        assert_eq!(
            wait_plan(
                parse_reset(
                    "usage limit reached; resets 2026-09-20",
                    last_minute_of_the_day()
                ),
                last_minute_of_the_day(),
                MARGIN,
                CEILING,
            ),
            WaitPlan::Deadline {
                at: datetime!(2026-09-20 00:01:00 UTC),
            },
            "a day boundary read from the last minute of the day is a minute of \
             wait plus the margin, not a wait that overshoots into the next day",
        );
    }

    #[test]
    fn a_clock_time_still_ahead_lands_on_the_day_it_was_written() {
        assert_eq!(
            parse_reset(
                "ERROR: usage limit reached; retry after 14:05 UTC",
                morning()
            ),
            Some(datetime!(2026-09-19 14:05:00 UTC)),
            "the same clock time twice a day is resolved by the instant it was \
             read at, and this one has not been reached yet",
        );
    }

    #[test]
    fn a_clock_time_already_past_rolls_across_midnight_to_the_next_day() {
        // Pin the boundary the other way as well: a clock read at the very
        // instant it names is that instant, not the same clock time one day
        // away. Rolling a deadline that is already due is a whole day spent
        // waiting for a limit that has already lifted.
        assert_eq!(
            parse_reset("usage limit reached; resets at 09:00 UTC", morning()),
            Some(morning()),
        );
        assert_eq!(
            parse_reset(
                "usage limit reached; resets at 00:00:00Z",
                last_minute_of_the_day()
            ),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
            "midnight read at 23:59:30 is thirty seconds away: reading it as the \
             midnight that has just passed would wait out a whole day for a limit \
             that has already lifted",
        );
    }

    #[test]
    fn a_clock_time_rolls_the_year_over_at_the_last_moment_of_the_last_day() {
        assert_eq!(
            parse_reset(
                "usage limit reached; resets at 00:00 UTC",
                datetime!(2026-12-31 23:59:59 UTC)
            ),
            Some(datetime!(2027-01-01 00:00:00 UTC)),
            "the day after the last day of a year is a day, and a rollover that \
             stops at 31 December would wake a year early",
        );
    }

    #[test]
    fn a_clock_time_that_is_not_made_a_deadline_is_not_a_reset() {
        assert_eq!(
            parse_reset("[09:12:33] usage limit reached for this plan", morning()),
            None,
            "a line of log output is full of clock times that say when a line was \
             printed, and waiting to one of them is a wait nobody asked for",
        );
    }

    #[test]
    fn a_clock_time_written_in_the_meridian_is_not_a_reset() {
        assert_eq!(
            parse_reset("usage limit reached; resets at 8:00 pm", morning()),
            None,
            "reading 8:00 pm as 08:00 would wake the run twelve hours early, so \
             a twelve-hour clock falls back to a bounded backoff instead",
        );
    }

    #[test]
    fn a_relative_duration_is_measured_from_the_instant_it_was_read_at() {
        assert_eq!(
            parse_reset("retry-after: 3600", morning()),
            Some(datetime!(2026-09-19 10:00:00 UTC)),
            "the HTTP header sends a bare number of seconds, and that number is \
             the whole answer",
        );
    }

    #[test]
    fn a_fractional_duration_keeps_its_fraction() {
        assert_eq!(
            parse_reset(
                "Rate limit reached for gpt-5.1-codex on tokens per min (TPM): Limit \
                 30000, Used 29998, Requested 900. Please try again in 1.8s.",
                morning(),
            ),
            Some(morning() + Duration::seconds(1) + Duration::nanoseconds(800_000_000)),
            "the tokens-per-minute refusal names a sub-second wait, and rounding \
             it up to a second is a different answer from the one sent",
        );
    }

    #[test]
    fn a_duration_written_in_several_parts_is_the_sum_of_them() {
        assert_eq!(
            parse_reset("usage limit reached; resets in 2h 15m", morning()),
            Some(datetime!(2026-09-19 11:15:00 UTC)),
        );
    }

    /// A form both CLIs write when the wait is not a round number.
    #[test]
    fn a_duration_written_without_a_separator_is_refused_rather_than_half_read() {
        assert_eq!(
            parse_reset("usage limit reached; resets in 1h30m", morning()),
            None,
            "neither `1h` nor `30m` is a word boundary inside `1h30m`, so the \
             whole token is refused; reading the `30m` half of it would wait \
             ninety minutes short of the promise the line made",
        );
    }

    #[test]
    fn a_relative_duration_that_crosses_midnight_lands_the_next_day() {
        assert_eq!(
            parse_reset(
                "usage limit reached; try again in 3 hours",
                datetime!(2026-09-19 23:00:00 UTC)
            ),
            Some(datetime!(2026-09-20 02:00:00 UTC)),
            "a wait measured from now crosses the day boundary where now asks it \
             to, and a day kept by truncating to midnight would lose three hours",
        );
    }

    #[test]
    fn an_absolute_day_is_read_before_a_duration_written_beside_it() {
        assert_eq!(
            parse_reset(
                "usage limit reached; resets 2026-09-20, retry-after: 3600",
                morning()
            ),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
            "an instant the provider named outranks a duration on the same line: \
             the ceiling lifts at the instant, not one hour after the question",
        );
    }

    #[test]
    fn a_day_that_does_not_exist_is_not_re_read_as_the_clock_inside_it() {
        assert_eq!(
            parse_reset(
                "usage limit reached; resets 2026-13-45T10:00:00Z; retry after 60s",
                morning()
            ),
            None,
            "a line that names a day no calendar holds has named no reset, and \
             reading the 10:00 or the 60s out of it would honour a malformed promise",
        );
    }

    #[test]
    fn a_reset_is_read_from_the_line_the_limit_was_named_on() {
        let text = "reading the queue\nERROR: usage limit reached; resets at \
                    2026-09-20T00:00:00Z\ndone\n";
        let line = limit_message(text, &[]).expect("the limit line is the answer T065 returns");
        assert_eq!(
            parse_reset(&line, morning()),
            Some(datetime!(2026-09-20 00:00:00 UTC)),
            "ADR-0060 returned the line because the reset time is the useful half \
             of it; this is the half being read",
        );
    }

    #[test]
    fn a_reset_that_cannot_be_placed_costs_a_bounded_backoff_and_never_an_unbounded_wait() {
        for line in [
            "You've hit your weekly limit; it will reset at 8pm (America/Los_Angeles) on Tuesday.",
            "HTTP 429 from api.anthropic.com",
            "usage limit reached; resets at midnight",
            "",
        ] {
            let reset = parse_reset(line, morning());
            assert_eq!(
                reset, None,
                "{line:?} names no instant this parser can place"
            );
            let plan = wait_plan(reset, morning(), MARGIN, CEILING);
            let WaitPlan::Backoff { wait } = plan else {
                panic!("an unplaced reset must never become a deadline: {line:?} gave {plan:?}");
            };
            assert!(
                wait > Duration::ZERO && wait <= CEILING,
                "{line:?} backed off for {wait:?}, which is neither a wait nor inside \
                 the ceiling the run gave itself",
            );
        }
    }

    #[test]
    fn a_known_reset_inside_the_ceiling_is_waited_out_to_the_instant_plus_margin() {
        let reset = datetime!(2026-09-19 13:00:00 UTC);
        assert_eq!(
            wait_plan(Some(reset), morning(), MARGIN, CEILING),
            WaitPlan::Deadline {
                at: datetime!(2026-09-19 13:01:00 UTC)
            },
            "VISION.md §7 waits a known reset out to the instant with margin, so a \
             limit that lifts a second early does not cost a second refusal",
        );
    }

    #[test]
    fn a_margin_of_nothing_waits_to_the_exact_instant_the_provider_named() {
        let reset = datetime!(2026-09-19 13:00:00 UTC);
        assert_eq!(
            wait_plan(Some(reset), morning(), Duration::ZERO, CEILING),
            WaitPlan::Deadline { at: reset },
        );
    }

    #[test]
    fn a_reset_the_provider_already_missed_becomes_a_backoff() {
        let missed = morning() - Duration::hours(1);
        assert_eq!(
            wait_plan(Some(missed), morning(), MARGIN, CEILING),
            WaitPlan::Backoff { wait: MARGIN },
            "a promise whose instant has passed is no longer a promise about the \
             future, and waking at it means waking now to ask again immediately",
        );
    }

    #[test]
    fn a_reset_beyond_the_ceiling_is_never_waited_out_as_a_deadline() {
        let far = morning() + Duration::days(5);
        assert_eq!(
            wait_plan(Some(far), morning(), MARGIN, CEILING),
            WaitPlan::Backoff { wait: MARGIN },
            "limit_max_wait_secs is the longest wait a run may sit through before \
             it reports a limit, and a five-day deadline is a run that appears to hang",
        );
    }

    #[test]
    fn a_margin_that_pushes_a_reset_past_the_ceiling_becomes_a_backoff() {
        let almost = morning() + CEILING - Duration::seconds(30);
        assert_eq!(
            wait_plan(Some(almost), morning(), MARGIN, CEILING),
            WaitPlan::Backoff { wait: MARGIN },
            "the ceiling is the ceiling whatever the margin does to it",
        );
    }

    #[test]
    fn no_reset_time_is_a_bounded_backoff() {
        assert_eq!(
            wait_plan(None, morning(), MARGIN, CEILING),
            WaitPlan::Backoff { wait: MARGIN },
            "an unknown reset uses bounded backoff (VISION.md §7); the configured \
             margin is the pause the configuration already means by asking again",
        );
    }

    #[test]
    fn a_margin_larger_than_the_ceiling_is_clamped_to_the_ceiling() {
        assert_eq!(
            wait_plan(None, morning(), Duration::days(2), CEILING),
            WaitPlan::Backoff { wait: CEILING },
        );
    }

    #[test]
    fn a_ceiling_of_nothing_waits_for_nothing() {
        for reset in [None, Some(morning() + CEILING)] {
            assert_eq!(
                wait_plan(reset, morning(), MARGIN, Duration::ZERO),
                WaitPlan::Backoff {
                    wait: Duration::ZERO
                },
                "a run that has allowed itself no wait at all is answered with no \
                 wait rather than with a deadline it refused to sit through",
            );
        }
    }

    /// Every shape a reset is read from is a regular expression.
    ///
    /// As with the classification tables, a shape that does not compile is
    /// skipped rather than panicking, which would otherwise be a rule that
    /// quietly stopped existing: every limit written in that shape would fall
    /// back to a bounded wait with no instant in it, and only a run that waited
    /// the wrong length would notice.
    #[test]
    fn every_reset_pattern_compiles() {
        for (name, pattern) in [
            ("reset day", &RESET_DAY),
            ("reset clock", &RESET_CLOCK),
            ("reset span", &RESET_SPAN),
            ("reset retry-after", &RESET_RETRY_AFTER),
        ] {
            assert!(
                pattern.regex().is_some(),
                "{name} is not a regular expression",
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

        /// A plan is always a finite wait: a deadline inside the ceiling it was
        /// handed, or an interval between zero and that ceiling. Nothing here
        /// can hand the caller a wait it has to put a watchdog on, whatever the
        /// provider wrote and whatever the two knobs are set to.
        #[test]
        fn a_wait_plan_never_waits_longer_than_the_ceiling_it_was_handed(
            stand in 0i64..315_360_000,
            reset_delta in -86_400i64..2_592_000,
            has_reset in any::<bool>(),
            margin in -60i64..259_200,
            ceiling in 0i64..259_200,
        ) {
            let now = datetime!(2026-01-01 00:00:00 UTC) + Duration::seconds(stand);
            let reset =
                has_reset.then(|| now + Duration::seconds(reset_delta));
            let ceiling = Duration::seconds(ceiling);
            let plan = wait_plan(reset, now, Duration::seconds(margin), ceiling);
            match plan {
                WaitPlan::Deadline { at } => {
                    prop_assert!(at > now, "a deadline at or behind now waits for nothing: {plan:?}");
                    prop_assert!(
                        at - now <= ceiling,
                        "a deadline past the ceiling hangs the run: {plan:?} > {ceiling:?}"
                    );
                }
                WaitPlan::Backoff { wait } => {
                    prop_assert!(!wait.is_negative(), "a negative wait is not a wait: {plan:?}");
                    prop_assert!(
                        wait <= ceiling.max(Duration::ZERO),
                        "a backoff past the ceiling hangs the run: {wait:?} > {ceiling:?}"
                    );
                }
            }
        }

        /// Every shape a reset is read from holds digits — a day, a clock, a
        /// number beside a unit. Prose alone, however clearly it complains about
        /// a limit, cannot become a wait.
        #[test]
        fn text_that_carries_no_digits_names_no_reset(text in "[a-z ]{0,120}") {
            prop_assert!(
                parse_reset(&text, morning()).is_none(),
                "invented a reset out of {text:?}",
            );
        }
    }
    /// Every unit word the duration shape accepts, beside the wait one of them
    /// names. `seconds_per_unit` is a table of magnitudes and nothing else in
    /// the suite reads a week, a microsecond, or the word `mins`: a length
    /// written wrong there is a run that waits by orders of magnitude, which is
    /// exactly the answer this table exists to keep from happening.
    #[test]
    fn every_duration_unit_means_the_length_the_table_says() {
        for (written, expected) in [
            ("1 ns", Duration::nanoseconds(1)),
            ("1 microsecond", Duration::microseconds(1)),
            ("2 us", Duration::microseconds(2)),
            ("3 milliseconds", Duration::milliseconds(3)),
            ("4 ms", Duration::milliseconds(4)),
            ("5 s", Duration::seconds(5)),
            ("6 sec", Duration::seconds(6)),
            ("7 secs", Duration::seconds(7)),
            ("8 second", Duration::seconds(8)),
            ("9 seconds", Duration::seconds(9)),
            ("1 m", Duration::seconds(60)),
            ("2 min", Duration::seconds(120)),
            ("3 mins", Duration::seconds(180)),
            ("4 minute", Duration::seconds(240)),
            ("5 minutes", Duration::seconds(300)),
            ("1 h", Duration::seconds(3_600)),
            ("2 hr", Duration::seconds(7_200)),
            ("3 hrs", Duration::seconds(10_800)),
            ("4 hour", Duration::seconds(14_400)),
            ("5 hours", Duration::seconds(18_000)),
            ("1 d", Duration::seconds(86_400)),
            ("2 day", Duration::seconds(172_800)),
            ("3 days", Duration::seconds(259_200)),
            ("1 w", Duration::seconds(604_800)),
            ("2 wk", Duration::seconds(1_209_600)),
            ("3 wks", Duration::seconds(1_814_400)),
            ("4 week", Duration::seconds(2_419_200)),
            ("5 weeks", Duration::seconds(3_024_000)),
        ] {
            assert_eq!(
                parse_reset(
                    &format!("usage limit reached; resets in {written}"),
                    morning(),
                ),
                Some(morning() + expected),
                "`{written}` is not the wait it names",
            );
        }
    }

    #[test]
    fn parts_joined_by_a_word_or_a_punctuation_mark_are_one_duration() {
        for line in [
            "usage limit reached; resets in 2h and 15m",
            "usage limit reached; resets in 2h, 15m",
            "usage limit reached; resets in 2h - 15m",
        ] {
            assert_eq!(
                parse_reset(line, morning()),
                Some(datetime!(2026-09-19 11:15:00 UTC)),
                "{line:?} names one wait written in two parts",
            );
        }
    }

    #[test]
    fn only_the_first_wait_on_a_line_is_summed() {
        assert_eq!(
            parse_reset("try again in 20s; the window resets in 4h", morning()),
            Some(datetime!(2026-09-19 09:00:20 UTC)),
            "a line that names two waits has one answer, and adding them would \
                 invent a deadline no provider sent",
        );
    }
}

//! The provider layer: the one trait a coding-agent CLI is reached through, and
//! the one shape its reporting takes.
//!
//! VISION.md §12 makes the provider layer a stable capability interface with
//! adapters behind it, and §6 makes every preserved attempt record carry
//! "tokens, and cost". Those two sentences have to speak through one type, and
//! this module owns it: [`Usage`].
//!
//! The rule the type is built around is that an unreported figure is `None`
//! beside [`UsageSource::Unavailable`], never `0`. A zero is a measurement: it
//! says the session spent nothing, which is a claim no adapter is entitled to
//! make about a session that reported nothing. Once a zero is in the record it
//! is indistinguishable from a genuinely free run, and every total it joins
//! inherits the substitution. VISION.md §17 ships the recording in v0.1 and
//! defers enforcement, which is exactly the trade the rule protects: a figure
//! written now outlives the code that reads it, and a later reader cannot tell
//! a substituted zero from a measured one.
//!
//! `docs/DESIGN.md` names the field under its `AttemptFinished` payload as
//! `usage: Option<Usage>`, so this is the durable half of the provider layer:
//! an adapter's parsing may change, the bytes already in a journal may not.
//! ADR-0049 records why unknown is `None` rather than the cheaper number.
//!
//! Above the reporting sits [`Provider`], and it is the whole of what the core
//! knows about a coding-agent CLI: a name, what startup detection found
//! ([`Capabilities`]), and one way to run a session — [`Invocation`] in,
//! [`Outcome`] out. VISION.md §12's adapters (`dummy`, Claude, Codex, and the
//! backlog behind them) are interchangeable because nothing above this module
//! can tell them apart: a caller holds `dyn Provider`, and every decision an
//! adapter's *identity* would answer is instead answered by a capability. A
//! `match` on a concrete adapter is the thing this trait exists to make
//! impossible — it is a decision re-made for every CLI added.
//!
//! Two derives, and the difference between them is deliberate.
//! [`Capabilities`] is what detection found, and `docs/DESIGN.md` journals it
//! under `ProviderDetected`, so it round-trips through JSON the way [`Usage`]
//! does. [`Invocation`] and [`Outcome`] are the call rather than a record of
//! it: no catalog entry names either whole — `AttemptFinished` names the
//! fields it wants out of an `Outcome` one at a time — so a serde derive on
//! them would fix an encoding nothing reads while dragging the prompt, which
//! is an `Invocation` field, into a journal that never asked for it.
//!
//! The module also owns the one rule VISION.md §12 states about models: the
//! configured id and the id a session reports are both recorded, and an
//! unexpected mismatch between them is rejected rather than tolerated. A rule
//! needs both halves to be comparable at all, so this file keeps each one where
//! it arrives — [`Invocation::model`] holds what was asked for and
//! [`Outcome::model_reported`] holds what the session said — while
//! [`check_model`] is the one place allowed to read them against each other.
//!
//! Below the trait sits [`build`], where a word in a configuration stops being
//! text and becomes the adapter a run holds. It is the one place in the
//! workspace allowed to name a concrete adapter, and it lives here rather than
//! wherever a CLI is reached because the paragraphs above are the reason:
//! interchangeability is what the trait buys, an adapter's identity answers
//! nothing above this module, and naming one is therefore the single decision of
//! its kind, to be made once from data an operator wrote.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::{Bus, Config, Error, Result};

pub mod claude;
pub mod codex;
// The one suite every adapter above is checked against. Compiled for tests only:
// its verdict is a panic, which is what an assertion is, while a run may not
// panic at all — the workspace lints make that an error rather than a style rule.
// Every adapter lives in this crate, so nothing outside it needs the suite either.
#[cfg(test)]
pub mod conformance;
pub mod dummy;
pub mod process;

/// Where the figures inside a [`Usage`] came from.
///
/// The source is part of the record rather than metadata. The same counts read
/// out of a session's own text and reported by the provider's telemetry are two
/// different facts about two different witnesses, and VISION.md §12's rule that
/// a provider's own report is the one to believe can only be applied by code
/// that can see which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSource {
    /// The provider reported the figures itself, through structured output or
    /// usage telemetry — the capability VISION.md §12 has startup detection
    /// record.
    Provider,
    /// The figures were read out of what the session printed. A parse is a
    /// claim about text, so a figure with this source is weaker evidence than
    /// the same figure from [`UsageSource::Provider`], and says so.
    ParsedFromOutput,
    /// Nothing was reported. Every figure of a [`Usage`] carrying this is
    /// `None`, and the only honest way to print one is as unknown.
    Unavailable,
}

/// What one provider session reported about what it spent.
///
/// Every figure is optional because every figure may not have been reported:
/// VISION.md §12 makes usage telemetry a detected capability rather than a
/// guarantee, so an adapter routinely finishes a session holding nothing. The
/// two states are kept apart by [`UsageSource`] rather than collapsed into a
/// number, and nothing in this type closes that gap by default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Tokens the provider counted as input, or `None` when it counted none
    /// such. `None` is not [`Some(0)`](Option::Some): a session that reported
    /// an empty prompt and a session that reported nothing are different facts.
    pub input_tokens: Option<u64>,
    /// Tokens the provider counted as output, with the same distinction as
    /// [`Usage::input_tokens`].
    pub output_tokens: Option<u64>,
    /// Tokens served out of the provider's cache. A subset of
    /// [`Usage::input_tokens`] — they are input tokens the provider did not
    /// have to recompute — so they are reported beside the input count and
    /// deliberately not added to it.
    pub cached_tokens: Option<u64>,
    /// What the session cost, in US dollars, as whoever reported it priced it.
    /// A float because that is the form both a provider's telemetry and a price
    /// table answer in; `None` when nobody priced the session.
    pub cost_usd: Option<f64>,
    /// Who reported the four figures above, and therefore how far to trust them.
    pub source: UsageSource,
}

impl Usage {
    /// The usage of an attempt whose provider reported nothing at all.
    ///
    /// This is the value an attempt with no telemetry gets. The alternative —
    /// a `Usage` of zeroes — would record that the run spent nothing, which is
    /// a claim about the session rather than about what was learned of it.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_usd: None,
            source: UsageSource::Unavailable,
        }
    }

    /// Input tokens plus output tokens, or `None` when either half is unknown.
    ///
    /// A total over a partially reported session is the zero substitution
    /// wearing a different hat: dropping the unknown half out of a sum reports
    /// a figure the attempt's own record does not support. So a session that
    /// reported one half and not the other has no total, and says so rather
    /// than reporting the half it happens to hold.
    ///
    /// [`Usage::cached_tokens`] is not added to the sum. Cached tokens are a
    /// subset of [`Usage::input_tokens`], so adding them would count the same
    /// tokens twice and inflate every figure a budget is later read from.
    ///
    /// A sum too large for `u64` answers `None` as well, by way of
    /// [`u64::checked_add`]. Unreachable for any session a provider has
    /// reported; the alternative is a wrapped total, and a wrapped total is a
    /// fabricated figure rather than an unknown one.
    #[must_use]
    pub const fn total_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => input.checked_add(output),
            _ => None,
        }
    }
}

impl Default for Usage {
    /// The default is [`Usage::unavailable`], not a zeroed struct.
    ///
    /// Reaching for a default means having nothing to record, which is the
    /// fact [`Usage::unavailable`] carries. A derived default would have had to
    /// invent a [`UsageSource`] for a session nobody reported, and the only
    /// variant a derive can pick is the first one — `Provider`, the most
    /// trusted source there is, attached to four zeroes nobody measured.
    fn default() -> Self {
        Self::unavailable()
    }
}

/// What startup detection found a provider able to do.
///
/// VISION.md §12 makes these three questions the first thing ktask asks a CLI,
/// and makes the answers a precondition rather than a hint: a provider is only
/// ever asked for what it was detected able to give. Each is a plain `bool`
/// because detection answers all three of a provider it ran detection on;
/// "never asked" is not a fourth answer this type can hold, and it is recorded
/// as the absence of the detection event instead.
///
/// A `false` is the conservative answer, so it is the one a partial detection
/// must not be allowed to imply silently: [`Capabilities`] refuses to
/// deserialise with a field missing, which is why every field is written even
/// when it says no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Whether the CLI can be asked for its reply as structured output rather
    /// than prose. A `true` is what lets an adapter read an outcome and its
    /// figures out of a document; a `false` means anything it reports about
    /// itself was parsed out of free text, which is what
    /// [`UsageSource::ParsedFromOutput`] exists to say.
    pub structured_output: bool,
    /// Whether a model id can be handed to it at all. An [`Invocation`]
    /// carrying `model: Some(..)` for a provider answering `false` cannot be
    /// honoured: the id would be dropped in silence, and the attempt record
    /// would then name a model the session never ran on — the mismatch
    /// VISION.md §12 requires rejecting rather than tolerating.
    pub model_selection: bool,
    /// Whether the provider reports its own token and cost figures. When this
    /// is `false` the only honest sources left for a [`Usage`] are
    /// [`UsageSource::ParsedFromOutput`] and [`UsageSource::Unavailable`], and
    /// a run of such sessions is one whose cost is unknown rather than cheap.
    pub usage_telemetry: bool,
}

/// One session's worth of work, handed to a provider.
///
/// This is a call, not a record: see the module documentation for why it
/// carries no serde derive. The three fields are everything an adapter needs
/// and nothing it may decide for itself — which protocol phase the prompt
/// belongs to, and what the task is, stay with the caller that built it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The prompt, in full. It is the caller's job to have assembled it from
    /// the task and the protocol phase, and the adapter's job to pass it on
    /// without editing it: a prompt an adapter rewrote is a prompt no later
    /// attempt can be compared against.
    pub prompt: String,
    /// The model to run, as configured, or `None` to run whatever the CLI's own
    /// default is. `None` is not "any model will do": it is the operator having
    /// expressed no preference, which an adapter records as the id it actually
    /// ran rather than as a chosen one.
    pub model: Option<String>,
    /// The directory to run in — a task's own worktree, never the main checkout
    /// (VISION.md §10). An adapter runs its CLI here and nowhere else; the
    /// isolation an attempt's evidence is attributed to comes from this field.
    pub working_dir: PathBuf,
}

/// What one session left behind.
///
/// Every field is what the session did or reported, which is the only kind of
/// answer this type may hold. Nothing here is inferred to fill a gap: an
/// absent [`Usage`] is a session that reported nothing, and an absent
/// `session_id` is one whose identity was never revealed.
/// It derives `PartialEq` and not `Eq` for the reason [`Usage`] carries no `Eq`
/// either: a `cost_usd` is a float, and an equality over floats cannot keep the
/// promise `Eq` makes.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The exit status of the CLI's own process, as it left it. It is what the
    /// process returned, never what the task achieved: VISION.md §3's invariant
    /// 4 forbids a task being done on an agent's exit code, so this is evidence
    /// a session ran and nothing more.
    pub exit_code: i32,
    /// Everything the session printed to be read as its work, in order, after
    /// the same capture limits a gate's output gets.
    pub stdout: String,
    /// Everything it printed to be read as a problem. Kept separate rather than
    /// merged, because the distinction is unrecoverable once merged and is what
    /// a failure classification reads first.
    pub stderr: String,
    /// What it said it spent, or `None` when nobody said. `None` and
    /// `Some(Usage::unavailable())` are different facts — no report, against an
    /// empty one — and only the second says the provider was asked.
    pub usage: Option<Usage>,
    /// The session's own identifier, when it disclosed one. It is recorded in
    /// attempt evidence and nothing depends on it: VISION.md §12 states that no
    /// correctness path relies on resuming a session, so an adapter that cannot
    /// produce one has lost nothing a task needs.
    pub session_id: Option<String>,
    /// The model the session said it ran on, or `None` when it said nothing.
    ///
    /// VISION.md §12 records this beside [`Invocation::model`] rather than in
    /// place of it: one is what configuration asked for and the other is what
    /// the CLI reported, and the two are only comparable while both survive the
    /// session. `docs/DESIGN.md` names this field under its `AttemptFinished`
    /// payload and under the `AttemptRecord` the flight recorder stores, so
    /// this is the half of the evidence of an attempt that outlives the run.
    ///
    /// `None` is the mark of a missing report and is never filled in. Writing
    /// the configured id here whenever a session said nothing would record an
    /// unmeasured session as one that had proved which model it ran on, which is
    /// the substitution ADR-0049 refuses for a token count. [`check_model`] is
    /// what reads the pair, and [`Capabilities::model_selection`] is what says
    /// whether a report was ever owed.
    pub model_reported: Option<String>,
}

/// A coding-agent CLI the runner can hand a task to.
///
/// Object safety is a requirement rather than an incidental property: the
/// runner holds `Box<dyn Provider>` or `&dyn Provider` so that which CLI runs a
/// task is a value that came from configuration. The moment the trait grows a
/// generic method or a `Self`-sized return, a caller has to know which adapter
/// it is holding, and interchangeability — the point of the whole layer — is
/// gone. `trait_tests` holds one such box, so the loss fails the build here
/// rather than in the runner.
///
/// Every method takes `&self`, so one adapter can serve a whole run, including
/// the review task a different provider was configured for, without a caller
/// holding `&mut` and excluding everyone else from it.
pub trait Provider {
    /// The name the operator writes in configuration and reads in the TUI —
    /// `dummy`, `claude`, `codex`. It is how a provider is identified in a
    /// [`crate::Error::Provider`], so it is a stable label rather than a display
    /// string a later version may reword.
    fn name(&self) -> &str;

    /// What detection found, which is what a caller is allowed to ask for.
    ///
    /// This is answered per call rather than stored by the caller so an adapter
    /// can answer from what it knows the moment it knows it — a CLI detected at
    /// startup, or one whose version changed under a long-lived run.
    fn capabilities(&self) -> Capabilities;

    /// Run one session, and report what it left.
    ///
    /// `bus` is the live view the TUI and `ktask-rs status` read. `Some(..)`
    /// means someone is watching: an implementation hands what the session
    /// prints to it as [`EventKind::AgentOutput`](crate::EventKind), the one
    /// catalog entry that exists to carry it, while the session is still
    /// running. `None` means nobody is listening, and changes nothing else
    /// about the call — a provider that behaves differently when watched is a
    /// provider whose scenario cannot be reproduced headlessly, which is what
    /// the scenario suite depends on.
    ///
    /// Redaction is not an adapter's job: the journal runs a payload through
    /// [`crate::redact`] on its way into a record, so a line carrying a
    /// credential is stored redacted whoever published it.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Provider`] when the CLI could not be started, could not be
    /// reached, or refused the work. A refusal is an error and never an
    /// `Outcome`: a process that never ran has no exit status to report, and
    /// inventing one is the substitution ADR-0049 forbids for a token count.
    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome>;
}

/// Refuse an attempt whose session ran on a model the configuration did not
/// choose.
///
/// VISION.md §12 both records the configured and reported ids and rejects an
/// unexpected mismatch, and this is the rejection. It is a call of its own
/// rather than a step inside [`Provider::invoke`], because the two halves come
/// from different directions and only the caller that accepts an attempt holds
/// both at once: the configured id arrives on [`Invocation::model`], the
/// reported one arrives on [`Outcome::model_reported`], and an adapter refusing
/// a session over the report it gave back would present a configuration fault as
/// a provider failure — the one confusion VISION.md §7 cannot retry out of.
///
/// Four cases, of which exactly one is refused:
///
/// * both present and equal — accepted, and both are recorded.
/// * both present and different — refused, before the attempt is accepted.
/// * configured and unreported — accepted, and the record keeps the gap:
///   [`Outcome::model_reported`] stays `None` instead of taking the configured
///   id. That is the mark VISION.md §12 asks a missing report to carry, and
///   [`Capabilities::model_selection`] is the detection that says a CLI owes no
///   report at all.
/// * nothing configured — accepted whatever was reported, because nothing was
///   contradicted; the answer the session gave is then the only record of what
///   ran.
///
/// Equality is exact. A prefix, a case difference, or a trailing space is a
/// different id, because the only mistake this rule can make in the lenient
/// direction is recording an attempt as having run on a model nobody chose — and
/// a recorded attempt is the evidence a later task is reasoned from.
///
/// # Errors
///
/// [`crate::Error::Config`] keyed `model` when both ids are present and differ,
/// naming both. A keyed `Config` error is how this core says a configured
/// setting cannot be honoured, which is
/// [`FailureClass::ProviderConfiguration`]: that class names an invalid model,
/// and VISION.md §7 pauses it for a human rather than spending another attempt.
/// Reaching the class from this error is the work of the classifier, and
/// ADR-0057 records that `classify` cannot see an error from a provider call yet.
///
/// [`FailureClass::ProviderConfiguration`]: crate::FailureClass::ProviderConfiguration
pub fn check_model(configured: Option<&str>, reported: Option<&str>) -> Result<()> {
    match (configured, reported) {
        (Some(configured), Some(reported)) if configured != reported => Err(Error::Config {
            key: "model".to_owned(),
            detail: format!(
                "the configuration asked for `{configured}` and the session reported \
                 `{reported}`; an attempt that ran on a model nobody chose is refused \
                 rather than recorded, and no retry can start the configured one"
            ),
        }),
        // One arm for three cases, because the rule separates only one of them:
        // an equal pair confirms what was configured, a missing report is marked
        // rather than refused, and an unconfigured run cannot be contradicted.
        _ => Ok(()),
    }
}

/// The word an operator writes for the scripted adapter, and the command its
/// sessions start.
const DUMMY_NAME: &str = "dummy";
/// The word an operator writes for the Claude CLI, and the command its sessions
/// start.
const CLAUDE_NAME: &str = "claude";
/// The word an operator writes for the Codex CLI, and the command its sessions
/// start.
const CODEX_NAME: &str = "codex";

/// Every provider word a configuration may name, in the order VISION.md §12
/// lists the launch set.
///
/// One list, read twice by [`build`]: as the words its arms match, and as the
/// names a refusal offers the human it stopped for. A refusal assembled out of a
/// second list would be a refusal that could be wrong, and a pause is only worth
/// what it costs if the answer it carries is the whole answer.
const PROVIDER_NAMES: [&str; 3] = [DUMMY_NAME, CLAUDE_NAME, CODEX_NAME];

/// The adapter a configuration names, built from nothing but that configuration.
///
/// VISION.md §12 makes which CLI runs a task a value — the word in
/// [`Config::provider`] — and this is where the word becomes the `Box<dyn
/// Provider>` a runner, a `doctor` check and a TUI screen all read the same way.
/// The trait above is what makes them able to; this function is what makes them
/// able to without knowing which one they hold. Adding one of §12's backlog CLIs
/// is the "additive" case, and it is one arm here and one name in the list of
/// valid names below, which is the whole of the change an adapter is allowed to
/// ask of the layer above it.
///
/// Three decisions the arms had to make, none of which an adapter can make for
/// itself.
///
/// * **`dummy` is its scenario file**, and the path is the whole of what it is
///   given. ADR-0051 put a scenario in a document precisely so that no script
///   could live in code, so a `dummy` whose setting names no file is refused
///   rather than handed a behavior invented here. What a named file gets wrong
///   comes back as the adapter's own refusal, unwritten.
/// * **The two real adapters get a command word and the two clocks.** `Config`
///   has no per-provider command key, so the word is the adapter's own name —
///   the CLI an operator means when they write `claude`, searched along `PATH`
///   by the adapter when a session starts (ADR-0054). Nothing is searched *for*
///   here: a missing CLI is refused as the configuration problem ADR-0054 says it
///   is, and a build that probed first could not tell an unconfigured machine
///   what it had been configured with. `idle_timeout_secs` and
///   `attempt_timeout_secs` (ADR-0053) arrive as constructor arguments rather
///   than as lookups, for the reason ADR-0054 gives: a session has to be
///   reproducible from the arguments its adapter was built with.
/// * **The match is exact.** No trim, no case fold, no prefix. A near miss
///   resolved into a real adapter runs the attempt on a provider nobody chose and
///   files its evidence under the wrong CLI, which is the mistake VISION.md §12
///   refuses a mismatched model id over rather than tolerating.
///
/// Two things this pointedly does not do. It does not weigh [`Config::model`]
/// against the chosen adapter's [`Capabilities::model_selection`]: ADR-0050 left
/// whether that is a refusal or a re-detection to a decision-maker, so it is
/// reported as an open question rather than answered here, and [`check_model`]
/// stays the one model rule this core holds. It also does not pick a provider per
/// task type (VISION.md §12's implement/review pair): that is a decision about a
/// task, and the caller holding the queue is the one allowed to make it.
///
/// # Errors
///
/// [`Error::Config`] keyed `provider` — naming the word it refused and every
/// valid name — when no adapter answers to the configured word; keyed
/// `dummy_scenario_path` when `dummy` is chosen with no scenario named. A named
/// scenario that will not load comes back unchanged: [`Error::Io`] for a path
/// holding nothing, [`Error::Config`] keyed by the file or the step for a
/// document that cannot replay as written. The reason an operator can act on is
/// the one the file gave, and this function owns none of it.
pub fn build(config: &Config) -> Result<Box<dyn Provider>> {
    let idle = Duration::from_secs(config.idle_timeout_secs);
    let attempt = Duration::from_secs(config.attempt_timeout_secs);
    match config.provider.as_str() {
        DUMMY_NAME => match config.dummy_scenario_path.as_deref() {
            Some(path) => Ok(Box::new(dummy::Dummy::load(path)?)),
            None => Err(Error::Config {
                key: "dummy_scenario_path".to_owned(),
                detail: format!(
                    "the `{DUMMY_NAME}` provider replays a scenario file and this \
                     configuration names none: set `dummy_scenario_path` to a TOML \
                     document of steps, or name another provider; no script is \
                     invented in its place"
                ),
            }),
        },
        CLAUDE_NAME => Ok(Box::new(claude::Claude::new(CLAUDE_NAME, idle, attempt))),
        CODEX_NAME => Ok(Box::new(codex::Codex::new(CODEX_NAME, idle, attempt))),
        unknown => Err(Error::Config {
            key: "provider".to_owned(),
            detail: format!(
                "`{unknown}` is not a provider this build has an adapter for; the valid \
                 names are {}",
                PROVIDER_NAMES.map(|name| format!("`{name}`")).join(", ")
            ),
        }),
    }
}

#[cfg(test)]
mod usage_tests {
    // Named for the type it holds rather than `tests`, so the assertions about
    // usage answer to `test(/provider::usage/)` on their own once the provider
    // trait arrives in this file and brings its own tests beside these.
    use super::{Usage, UsageSource};

    #[test]
    fn an_attempt_that_reported_nothing_has_no_figures_and_says_so() {
        let usage = Usage::unavailable();
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.source, UsageSource::Unavailable);
    }

    #[test]
    fn the_default_reports_nothing_rather_than_everything_at_zero() {
        let usage = Usage::default();
        assert_eq!(usage, Usage::unavailable());
        assert_eq!(usage.source, UsageSource::Unavailable);
        assert_eq!(usage.total_tokens(), None);
        assert_eq!(usage.cost_usd, None);
    }

    #[test]
    fn a_total_refuses_to_report_the_half_it_has() {
        let input_only = Usage {
            input_tokens: Some(1_000),
            output_tokens: None,
            cached_tokens: Some(600),
            cost_usd: Some(0.12),
            source: UsageSource::Provider,
        };
        assert_eq!(input_only.total_tokens(), None);

        let output_only = Usage {
            input_tokens: None,
            output_tokens: Some(250),
            ..input_only
        };
        assert_eq!(output_only.total_tokens(), None);
    }

    #[test]
    fn a_session_measured_at_zero_reports_a_total_of_zero() {
        // The counterpart to the rule above. Zero is a real answer when it was
        // measured, which is exactly why it must not stand in for the unknown.
        let usage = Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cached_tokens: Some(0),
            cost_usd: Some(0.0),
            source: UsageSource::Provider,
        };
        assert_eq!(usage.total_tokens(), Some(0));
    }

    #[test]
    fn a_total_adds_input_and_output_and_does_not_double_count_the_cache() {
        let usage = Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(250),
            cached_tokens: Some(600),
            cost_usd: None,
            source: UsageSource::ParsedFromOutput,
        };
        assert_eq!(usage.total_tokens(), Some(1_250));
    }

    #[test]
    fn a_total_too_large_for_the_field_is_unknown_rather_than_wrapped() {
        let usage = Usage {
            input_tokens: Some(u64::MAX),
            output_tokens: Some(1),
            cached_tokens: None,
            cost_usd: None,
            source: UsageSource::Provider,
        };
        assert_eq!(usage.total_tokens(), None);
    }

    #[test]
    fn an_unreported_usage_is_written_as_nulls_and_not_zeros() {
        let encoded =
            serde_json::to_string(&Usage::unavailable()).expect("a usage is serialisable");
        assert!(encoded.contains("\"input_tokens\":null"), "{encoded}");
        assert!(encoded.contains("\"output_tokens\":null"), "{encoded}");
        assert!(encoded.contains("\"cached_tokens\":null"), "{encoded}");
        assert!(encoded.contains("\"cost_usd\":null"), "{encoded}");
        assert!(encoded.contains("\"Unavailable\""), "{encoded}");
        assert!(!encoded.contains('0'), "a zero was substituted: {encoded}");
    }

    #[test]
    fn a_usage_round_trips_through_json_with_its_source_attached() {
        let usage = Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(250),
            cached_tokens: Some(600),
            cost_usd: Some(0.047_5),
            source: UsageSource::ParsedFromOutput,
        };
        let encoded = serde_json::to_string(&usage).expect("a usage is serialisable");
        let decoded: Usage = serde_json::from_str(&encoded).expect("the encoding is read back");
        assert_eq!(decoded, usage);
        assert_eq!(decoded.source, UsageSource::ParsedFromOutput);
        assert_eq!(decoded.total_tokens(), Some(1_250));
    }

    #[test]
    fn a_journal_entry_that_invents_a_field_is_refused() {
        let text = r#"{"input_tokens":1,"output_tokens":2,"cached_tokens":null,
             "cost_usd":null,"source":"Provider","reset_at":"2026-09-19"}"#;
        assert!(serde_json::from_str::<Usage>(text).is_err());
    }

    #[test]
    fn the_same_figures_from_two_sources_are_two_different_facts() {
        let reported = Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            cached_tokens: None,
            cost_usd: Some(0.01),
            source: UsageSource::Provider,
        };
        let parsed = Usage {
            source: UsageSource::ParsedFromOutput,
            ..reported
        };
        assert_ne!(reported, parsed);
        assert_eq!(parsed.input_tokens, reported.input_tokens);
    }
}

#[cfg(test)]
mod trait_tests {
    // Named for what it holds, the way `usage_tests` is named for `Usage`, so
    // the assertions about the interface answer to `test(/provider::trait/)` on
    // their own now that the trait shares this file with the usage type.
    use super::{Capabilities, Invocation, Outcome, Provider};
    use crate::{AttemptId, Bus, Error, Event, EventKind, EventSeq, Result, Stream, Usage};
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use time::macros::datetime;

    /// What detection found for an adapter that can answer none of the three
    /// questions — the truthful report of a CLI that does none of it.
    const DETECTED_NOTHING: Capabilities = Capabilities {
        structured_output: false,
        model_selection: false,
        usage_telemetry: false,
    };

    /// Structured replies and a selectable model, but no usage telemetry: the
    /// common shape, and the one that keeps a want for telemetry unsatisfiable
    /// here so the selection test can assert a refusal.
    const REPORTS_OUTPUT_NOT_USAGE: Capabilities = Capabilities {
        structured_output: true,
        model_selection: true,
        usage_telemetry: false,
    };

    /// What a [`Scripted`] adapter does when it is invoked.
    #[derive(Clone)]
    enum Reply {
        /// Run, answer with this, and hand the session's stdout to a listening
        /// bus as one `AgentOutput`. Splitting output into lines is the job of
        /// the process supervision to come, not of this interface.
        Ran(Outcome),
        /// Refuse to start. There is no exit code to report: a process that
        /// never ran has no status to carry.
        Refused(&'static str),
    }

    /// The one adapter these tests script, and deliberately not the `dummy`
    /// provider that replays scenarios: this exists to hold the interface still
    /// while a caller is asserted against it.
    ///
    /// It keeps every [`Invocation`] it was handed, so an assertion can read
    /// back what actually crossed the trait boundary rather than trust the
    /// signature that says what crosses it.
    struct Scripted {
        provider_name: &'static str,
        detected: Capabilities,
        reply: Reply,
        handed: RefCell<Vec<Invocation>>,
    }

    impl Scripted {
        fn new(provider_name: &'static str, detected: Capabilities, reply: Reply) -> Self {
            Self {
                provider_name,
                detected,
                reply,
                handed: RefCell::new(Vec::new()),
            }
        }

        /// Every invocation this adapter was handed, oldest first.
        fn handed(&self) -> Vec<Invocation> {
            self.handed.borrow().clone()
        }
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            self.provider_name
        }

        fn capabilities(&self) -> Capabilities {
            self.detected
        }

        fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
            self.handed.borrow_mut().push(inv.clone());
            match &self.reply {
                Reply::Refused(detail) => Err(Error::Provider {
                    provider: self.provider_name.to_owned(),
                    detail: (*detail).to_owned(),
                }),
                Reply::Ran(outcome) => {
                    if let Some(bus) = bus {
                        // No sequence of its own: the journal stamps one as the
                        // record is appended, which is what `Event::seq` says a
                        // producer must never guess at.
                        bus.publish(Event {
                            seq: EventSeq::new(0),
                            ts: datetime!(2026-09-19 12:00:00 UTC),
                            task_id: None,
                            kind: EventKind::AgentOutput {
                                attempt: AttemptId::new(1),
                                stream: Stream::Stdout,
                                text: outcome.stdout.clone(),
                            },
                        });
                    }
                    Ok(outcome.clone())
                }
            }
        }
    }

    /// A second, unrelated adapter: one that runs, says nothing, and reports
    /// nothing. Two implementations of two types is what makes "interchangeable"
    /// an assertion rather than a claim about two values of one type.
    struct Silent;

    impl Provider for Silent {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn capabilities(&self) -> Capabilities {
            DETECTED_NOTHING
        }

        fn invoke(&self, _inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
            Ok(Outcome {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                usage: None,
                session_id: None,
                model_reported: None,
            })
        }
    }

    /// One invocation with every field filled, because a call that leaves a
    /// field empty proves nothing about the one a caller depends on.
    fn invocation() -> Invocation {
        Invocation {
            prompt: "implement T054 and run the gates".to_owned(),
            model: Some("gpt-5.6-sol".to_owned()),
            working_dir: PathBuf::from("/state/ktask/ktask-rs/worktrees/54"),
        }
    }

    /// The outcome of a session that ran, printed one line, and reported its
    /// token counts, the session it ran in, and the model it ran on. The id is
    /// the one `invocation()` asks for, so the pair the fixture hands the
    /// caller is an accepted one rather than a mismatch.
    fn ran() -> Outcome {
        Outcome {
            exit_code: 0,
            stdout: "planning the fix\n".to_owned(),
            stderr: String::new(),
            usage: Some(Usage {
                input_tokens: Some(1_000),
                output_tokens: Some(250),
                cached_tokens: None,
                cost_usd: Some(0.0475),
                source: crate::UsageSource::Provider,
            }),
            session_id: Some("0f0f-1e1e".to_owned()),
            model_reported: Some("gpt-5.6-sol".to_owned()),
        }
    }

    /// Everything the core knows how to ask of a provider: hold them together,
    /// ask each what it can do, and use the one that answers yes. It lives here
    /// rather than in this module because no task has asked for a provider
    /// registry yet — what it proves is that writing one needs no `match` on a
    /// concrete adapter.
    fn capable<'providers>(
        providers: &[&'providers dyn Provider],
        want: Capabilities,
    ) -> Option<&'providers dyn Provider> {
        providers.iter().copied().find(|provider| {
            let have = provider.capabilities();
            (!want.structured_output || have.structured_output)
                && (!want.model_selection || have.model_selection)
                && (!want.usage_telemetry || have.usage_telemetry)
        })
    }

    #[test]
    fn every_message_a_provider_answers_arrives_through_a_trait_object() {
        let scripted = Scripted::new("codex", REPORTS_OUTPUT_NOT_USAGE, Reply::Ran(ran()));
        // The box is the first assertion: it compiles only while the trait is
        // object safe, so a generic method or a `Self`-sized return would stop
        // this test building rather than quietly become a caller's problem.
        let provider: Box<dyn Provider> = Box::new(scripted);

        assert_eq!(provider.name(), "codex");
        assert_eq!(provider.capabilities(), REPORTS_OUTPUT_NOT_USAGE);

        let outcome = provider
            .invoke(&invocation(), None)
            .expect("a scripted provider runs");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "planning the fix\n");
        assert_eq!(outcome.session_id.as_deref(), Some("0f0f-1e1e"));
        assert_eq!(
            outcome.usage.map(|usage| usage.total_tokens()),
            Some(Some(1_250)),
            "the usage a session reported survives the trip through the trait"
        );
    }

    #[test]
    fn an_invocation_arrives_with_every_field_the_caller_filled() {
        let scripted = Scripted::new("claude", REPORTS_OUTPUT_NOT_USAGE, Reply::Ran(ran()));
        let provider: &dyn Provider = &scripted;
        let asked = invocation();
        let later = Invocation {
            prompt: "review the candidate commit".to_owned(),
            ..invocation()
        };

        provider
            .invoke(&asked, None)
            .expect("a scripted provider runs");
        provider
            .invoke(&later, None)
            .expect("the same handle serves a second session");

        assert_eq!(
            scripted.handed(),
            vec![asked.clone(), later],
            "prompt, model and working directory all have to cross the trait \
             boundary intact, in the order they were asked, with `&self` alone \
             asked to change nothing"
        );
        assert_eq!(scripted.handed()[1].prompt, "review the candidate commit");
        assert_eq!(
            scripted.handed()[0].working_dir,
            Path::new("/state/ktask/ktask-rs/worktrees/54")
        );
    }

    #[test]
    fn providers_are_chosen_by_capability_and_never_by_their_type() {
        let talkative = Scripted::new("codex", REPORTS_OUTPUT_NOT_USAGE, Reply::Ran(ran()));
        let held: Vec<&dyn Provider> = vec![&Silent, &talkative];

        let wants_structured = Capabilities {
            structured_output: true,
            ..DETECTED_NOTHING
        };
        let chosen = capable(&held, wants_structured).expect("one of the two was detected able");
        // What the choice is used for is the point: nothing above the choice
        // knows, or can find out, which adapter it picked.
        assert_eq!(chosen.name(), "codex");
        assert_eq!(
            chosen
                .invoke(&invocation(), None)
                .expect("it runs")
                .exit_code,
            0
        );

        let wants_telemetry = Capabilities {
            usage_telemetry: true,
            ..DETECTED_NOTHING
        };
        assert!(
            capable(&held, wants_telemetry).is_none(),
            "neither adapter was detected able to report usage, and handing the \
             work to one anyway would record a figure nobody reported"
        );
        assert_eq!(
            capable(&held, DETECTED_NOTHING).map(Provider::name),
            Some("dummy"),
            "work that asks for nothing is runnable by a provider that can do nothing"
        );
    }

    #[test]
    fn what_a_session_printed_reaches_a_listening_bus_as_agent_output() {
        let bus = Bus::new();
        let mut watcher = bus.subscribe();
        let scripted = Scripted::new("codex", REPORTS_OUTPUT_NOT_USAGE, Reply::Ran(ran()));
        let provider: &dyn Provider = &scripted;

        let outcome = provider
            .invoke(&invocation(), Some(&bus))
            .expect("a scripted provider runs");

        let (events, dropped) = watcher.drain();
        assert_eq!(dropped, 0, "one line cannot overflow a live view");
        assert_eq!(events.len(), 1, "the session said one thing, once");
        let EventKind::AgentOutput {
            attempt,
            stream,
            text,
        } = &events[0].kind
        else {
            panic!(
                "a provider's output is an AgentOutput, not {:?}",
                events[0].kind
            );
        };
        assert_eq!(*attempt, AttemptId::new(1));
        assert_eq!(*stream, Stream::Stdout);
        assert_eq!(
            text, &outcome.stdout,
            "the bytes a viewer was shown and the bytes the caller was given are the same bytes"
        );
    }

    #[test]
    fn a_provider_answers_the_same_way_with_no_one_listening() {
        let scripted = Scripted::new("codex", REPORTS_OUTPUT_NOT_USAGE, Reply::Ran(ran()));
        let provider: &dyn Provider = &scripted;
        let bus = Bus::new();
        let mut viewer = bus.subscribe();

        let watched = provider
            .invoke(&invocation(), Some(&bus))
            .expect("a provider runs for a watching frontend");
        let (heard, _) = viewer.drain();
        assert_eq!(heard.len(), 1, "the watched call was heard by the bus");
        let alone = provider
            .invoke(&invocation(), None)
            .expect("the same provider runs the same way for no one");

        assert_eq!(alone, watched, "a live view is a listener, not an input");
        let (events, dropped) = viewer.drain();
        assert!(
            events.is_empty() && dropped == 0,
            "the unwatched call published nothing, as it had nowhere to publish to \
             ({} events, {dropped} dropped)",
            events.len()
        );
    }

    #[test]
    fn a_provider_that_never_started_answers_with_an_error_and_no_outcome() {
        let scripted = Scripted::new(
            "claude",
            REPORTS_OUTPUT_NOT_USAGE,
            Reply::Refused("the CLI is not on PATH"),
        );
        let provider: &dyn Provider = &scripted;

        let error = provider
            .invoke(&invocation(), None)
            .expect_err("this adapter was built to refuse");

        assert!(matches!(error, Error::Provider { .. }));
        assert_eq!(
            error.to_string(),
            "provider `claude` failed: the CLI is not on PATH",
            "the failure names the adapter that refused, which is what a \
             preflight and a human read"
        );
    }

    #[test]
    fn an_absent_usage_report_is_not_an_empty_one() {
        let silent = Silent
            .invoke(&invocation(), None)
            .expect("an adapter that can do nothing still runs work that asks for nothing");

        assert_eq!(silent.usage, None, "nobody reported anything");
        assert_eq!(silent.session_id, None, "and nobody named a session either");
        assert_ne!(
            silent.usage,
            Some(Usage::unavailable()),
            "`None` is no report and `Some(unavailable())` is an empty one; the \
             two are different facts about a session, as ADR-0049 holds for every \
             figure inside a Usage"
        );
    }

    #[test]
    fn a_capability_record_answers_all_three_questions_or_is_not_read() {
        let encoded = serde_json::to_string(&REPORTS_OUTPUT_NOT_USAGE)
            .expect("a capability record is journable, being what ProviderDetected carries");
        let decoded: Capabilities =
            serde_json::from_str(&encoded).expect("and reads back as the same answer");
        assert_eq!(decoded, REPORTS_OUTPUT_NOT_USAGE);
        assert!(
            encoded.contains("\"usage_telemetry\":false"),
            "a false is written as a false, not left out: {encoded}"
        );

        let unanswered = r#"{"structured_output":true,"model_selection":true}"#;
        let error = serde_json::from_str::<Capabilities>(unanswered)
            .expect_err("a record that never answered telemetry is not a capability record");
        assert!(
            error.to_string().contains("usage_telemetry"),
            "the refusal names the question that went unanswered: {error}"
        );

        let invented = r#"{"structured_output":true,"model_selection":true,
             "usage_telemetry":false,"approval_modes":["full"]}"#;
        assert!(
            serde_json::from_str::<Capabilities>(invented).is_err(),
            "a capability nobody detected is refused, not accepted and ignored"
        );
    }
}

#[cfg(test)]
mod model_check {
    // Which model an attempt ran on, and the one pairing of the two ids that
    // refuses it. Named for the rule it holds rather than for a type as
    // `usage_tests` and `trait_tests` are, because the rule is a function and
    // the selector this task is verified with is `test(/provider::model_check/)`.
    //
    // Every case is driven through the two boundary types that carry the halves
    // — an `Invocation` model and an `Outcome` model — rather than through bare
    // string literals, because what a later task wires up is the pair as those
    // types hold it. A test of two literals would still pass if the types had
    // nowhere to put a reported model, and that is the half of "both are
    // recorded" this file owes.
    use super::{Invocation, Outcome, check_model};
    use crate::{Error, Result};
    use std::path::PathBuf;

    /// The call of an attempt configured to run `id`, or to express no
    /// preference at all when `id` is `None`.
    fn configured(id: Option<&str>) -> Invocation {
        Invocation {
            prompt: "implement T062 and run the gates".to_owned(),
            model: id.map(str::to_owned),
            working_dir: PathBuf::from("/state/ktask/ktask-rs/worktrees/62"),
        }
    }

    /// The outcome of a session that ran, printed one line, and reported `id`
    /// as the model it ran on — or reported nothing when `id` is `None`.
    fn session(id: Option<&str>) -> Outcome {
        Outcome {
            exit_code: 0,
            stdout: "read the task, wrote the fix, ran the gates\n".to_owned(),
            stderr: String::new(),
            usage: None,
            session_id: None,
            model_reported: id.map(str::to_owned),
        }
    }

    /// Read the two halves out of the boundary types and check them, the way the
    /// runner that accepts an attempt will.
    fn check(inv: &Invocation, outcome: &Outcome) -> Result<()> {
        check_model(inv.model.as_deref(), outcome.model_reported.as_deref())
    }

    /// The shape every mismatch in this module must come back as, and the
    /// detail the assertions below are made against.
    fn refusal(error: &Error) -> String {
        let Error::Config { key, detail } = error else {
            panic!("a model mismatch is a provider-configuration refusal, got {error:?}");
        };
        assert_eq!(
            key, "model",
            "the refusal is keyed to the setting that cannot be honoured, which \
             is how a refused configuration is told apart from a session that \
             could not start"
        );
        detail.clone()
    }

    #[test]
    fn two_halves_that_agree_are_both_recorded_and_the_attempt_is_accepted() {
        let inv = configured(Some("gpt-5.6-sol"));
        let outcome = session(Some("gpt-5.6-sol"));

        check(&inv, &outcome)
            .expect("a session reporting the model it was asked to run is accepted");
        assert_eq!(
            outcome.model_reported, inv.model,
            "an accepted attempt records one model, so both halves have to \
             survive the check to be recorded beside each other"
        );
    }

    #[test]
    fn a_reported_model_that_differs_is_refused_before_the_attempt_is_accepted() {
        let inv = configured(Some("gpt-5.6-sol"));
        let outcome = session(Some("gpt-5.2-codex"));

        let error = check(&inv, &outcome).expect_err(
            "VISION.md section 12 rejects an unexpected mismatch rather than tolerating it",
        );
        let detail = refusal(&error);
        assert!(
            detail.contains("asked for `gpt-5.6-sol`"),
            "the refusal names the configured id as the configured id: {detail}"
        );
        assert!(
            detail.contains("reported `gpt-5.2-codex`"),
            "and the session id as the answer the session gave, since swapping \
             the two would send a human to change the wrong setting: {detail}"
        );
    }

    #[test]
    fn a_session_that_reports_no_model_is_accepted_and_recorded_as_unreported() {
        let inv = configured(Some("gpt-5.6-sol"));
        let outcome = session(None);

        check(&inv, &outcome).expect(
            "a provider that reports nothing is allowed: model reporting is a \
             detected capability (VISION.md section 12) and not a guarantee, so \
             refusing every session of one would make it unusable",
        );
        assert_eq!(
            outcome.model_reported, None,
            "the missing report stays missing instead of being filled in with \
             the configured id, which is the rule ADR-0049 states about a guess \
             standing in for a report, applied to a model id"
        );
        assert_eq!(
            inv.model.as_deref(),
            Some("gpt-5.6-sol"),
            "and the configured half is still recorded, so the gap sits beside \
             what was asked for rather than being the whole record"
        );
    }

    #[test]
    fn a_reported_model_with_nothing_configured_is_the_model_that_ran() {
        // Nothing was asked for, so nothing can be contradicted, and the answer
        // the session gave is the only record of what ran. Dropping it here
        // would empty the flight recorder for every default-model run.
        let inv = configured(None);
        let outcome = session(Some("provider-default-2026-09"));

        check(&inv, &outcome).expect("nothing was configured, so nothing was contradicted");
        assert_eq!(
            outcome.model_reported.as_deref(),
            Some("provider-default-2026-09"),
            "the report is what the evidence of an attempt records, configured \
             `None` or not"
        );
    }

    #[test]
    fn an_attempt_that_learned_nothing_about_either_id_is_accepted() {
        let inv = configured(None);
        let outcome = session(None);

        check(&inv, &outcome)
            .expect("a run that configured no model and reported no model has nothing to mismatch");
        assert_eq!(
            outcome.model_reported, inv.model,
            "both halves are recorded, as the nothing they both are"
        );
    }

    #[test]
    fn a_mismatch_is_refused_however_close_the_two_ids_look() {
        // The strictness is the point. Every pair below is one a lenient
        // comparison could wave through — a prefix, a case fold, a trailing
        // space — and each one waved through records an attempt as having run
        // on a model the configuration never chose.
        const CLOSE_CALLS: [(&str, &str); 3] = [
            ("gpt-5.6-sol", "gpt-5.6-sol-2026-09-01"),
            ("gpt-5.6-sol", "GPT-5.6-SOL"),
            ("gpt-5.6-sol", "gpt-5.6-sol "),
        ];
        for (asked, reported) in CLOSE_CALLS {
            let error = check_model(Some(asked), Some(reported))
                .expect_err("`{asked}` and `{reported}` are two different ids");
            let detail = refusal(&error);
            assert!(
                detail.contains(asked) && detail.contains(reported),
                "the refusal names both ids it compared: {detail}"
            );
        }
    }
}

#[cfg(test)]
mod build_tests {
    // Which adapter a run gets, and the one way it is chosen: a word in a
    // configuration. Named for the function it holds, the way `usage_tests` and
    // `trait_tests` are named for what they hold, so the factory answers to
    // `test(/provider::build/)` on its own.
    //
    // Every case goes through `build` and then through `Provider`, and no case
    // names an adapter type. That is the point rather than a constraint: a test
    // that built a `Dummy` to check that `build` returns a `Dummy` would still
    // pass while the factory handed back something else, and a factory returning
    // the wrong adapter is the failure this function has — it runs a real CLI
    // where an operator asked for a scripted one, and reports every attempt to
    // the wrong provider while it does.
    use super::build;
    use crate::{Config, Error, Invocation};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The providers VISION.md §12 puts at launch, in the order §12 lists them.
    const LAUNCH_NAMES: [&str; 3] = ["dummy", "claude", "codex"];

    /// A configuration that names `provider` and leaves every other setting at
    /// the `docs/DESIGN.md` default.
    fn configured(provider: &str) -> Config {
        // Assigned rather than built with `..Config::default()`: a `Config`
        // carries its key provenance in a private field, and the defaults are
        // the layer every other layer overrides, so there is nothing below them
        // for a test to claim.
        let mut config = Config::default();
        config.provider = provider.to_owned();
        config
    }

    /// Writes a one-step scenario into `dir` and returns the path it wrote.
    ///
    /// A document rather than an assembled [`Scenario`], because the setting
    /// under test holds a *path*: the file has to be the thing the adapter was
    /// built from, or the test would pass on a script `build` invented.
    fn scenario(dir: &Path, stdout: &str) -> PathBuf {
        let path = dir.join("scenario.toml");
        fs::write(
            &path,
            format!("steps = [{{ on_task = 1, outcome = \"success\", stdout = {stdout:?} }}]\n"),
        )
        .expect("the scratch directory accepts the scenario");
        path
    }

    /// The call a factory-built adapter is handed: a prompt, no model
    /// preference, and a directory of its own.
    fn session(worktree: &Path) -> Invocation {
        Invocation {
            prompt: "run the session the factory-built provider was handed".to_owned(),
            model: None,
            working_dir: worktree.to_path_buf(),
        }
    }

    /// The refusal `build` answers `config` with.
    ///
    /// A helper rather than `Result::expect_err`, which a `Box<dyn Provider>`
    /// cannot answer to: the trait has no `Debug` deliberately — an adapter is
    /// not a record — so the failure text has to come out of the adapter's own
    /// name rather than out of a debug format.
    fn refusal(config: &Config) -> Error {
        match build(config) {
            Ok(provider) => panic!(
                "`{}` was built as `{}` rather than refused",
                config.provider,
                provider.name()
            ),
            Err(error) => error,
        }
    }

    #[test]
    fn every_launch_name_reaches_an_adapter_that_answers_to_it() {
        // The ledger §12's launch set is measured against. A name that reaches
        // no adapter is a configuration that cannot be run; a name that reaches
        // the wrong one is worse, because it runs.
        let scratch = tempfile::tempdir().expect("a place to write a scenario");
        let mut config = configured("dummy");
        config.dummy_scenario_path = Some(scenario(
            scratch.path(),
            "one step is enough to be reachable\n",
        ));

        for name in LAUNCH_NAMES {
            config.provider = name.to_owned();
            let provider = build(&config).unwrap_or_else(|error| {
                panic!("`{name}` is a launch provider and was refused: {error}")
            });
            assert_eq!(
                provider.name(),
                name,
                "`{name}` reaches the adapter that answers to `{name}` — and note the \
                 scenario path is still set, so a `claude` or `codex` built here would \
                 have had to read a `dummy` file to be the wrong thing twice over"
            );
        }
    }

    #[test]
    fn the_dummy_variant_replays_the_scenario_its_configuration_names() {
        let scratch = tempfile::tempdir().expect("a place to write a scenario");
        let worktree = tempfile::tempdir().expect("a directory to replay into");
        let mut config = configured("dummy");
        config.dummy_scenario_path = Some(scenario(
            scratch.path(),
            "the file the setting pointed at\n",
        ));

        let provider = build(&config).expect("`dummy` with a scenario path is buildable");
        let outcome = provider
            .invoke(&session(worktree.path()), None)
            .expect("and the session its file scripts runs");

        assert_eq!(
            outcome.stdout, "the file the setting pointed at\n",
            "the session answered out of the file `dummy_scenario_path` named, so the \
             factory read the setting rather than replaying a script it wrote itself \
             — which is what ADR-0051 put the responses in a file to prevent"
        );
    }

    #[test]
    fn a_dummy_pointed_at_a_path_holding_nothing_reports_the_reason_the_file_gave() {
        let scratch = tempfile::tempdir().expect("a directory with nothing in it");
        let mut config = configured("dummy");
        config.dummy_scenario_path = Some(scratch.path().join("nowhere.toml"));

        let error = refusal(&config);
        assert!(
            matches!(error, Error::Io(..)),
            "the OS's own reason is the answer an operator can act on, and the factory \
             adds nothing to it, got {error}"
        );
    }

    #[test]
    fn a_dummy_with_no_scenario_named_is_refused_naming_the_setting() {
        // The documented default state: `provider = "dummy"`, no scenario. The
        // honest answer is a refusal naming the key to set, not a script
        // invented here — ADR-0051 keeps a scenario a file precisely so nothing
        // can be replayed that an operator never wrote down.
        let error = refusal(&Config::default());
        let Error::Config { key, detail } = &error else {
            panic!("a setting that cannot be honoured is a configuration refusal, got {error}");
        };
        assert_eq!(
            key, "dummy_scenario_path",
            "the refusal is keyed to the key that has to be set, which is how a human \
             reaches the right line of the right file"
        );
        assert!(
            detail.contains("dummy") && detail.contains("scenario"),
            "and it says both which provider needs the setting and what is missing: {detail}"
        );
    }

    #[test]
    fn an_unknown_name_is_refused_naming_it_and_listing_every_valid_one() {
        let error = refusal(&configured("kiro"));
        let Error::Config { key, detail } = &error else {
            panic!("a provider word nothing can honour is a configuration refusal, got {error}");
        };
        assert_eq!(
            key, "provider",
            "the refusal is keyed to the word it refused"
        );
        assert!(
            detail.contains("`kiro`"),
            "the refusal quotes the word as it was written, since that is what the \
             operator has to compare against: {detail}"
        );
        for name in LAUNCH_NAMES {
            assert!(
                detail.contains(name),
                "and it lists `{name}` among the valid names: a pause that stops for a \
                 human has to carry the whole answer, and the list is in this function \
                 alone: {detail}"
            );
        }
    }

    #[test]
    fn a_name_is_matched_exactly_and_a_near_miss_is_a_finding_not_a_choice() {
        // Every near-miss below is one a lenient comparison — trim, case fold,
        // prefix — would resolve into a real adapter. Choosing an adapter then
        // is running the attempt on a provider nobody configured, which is the
        // same class of mistake §12 refuses a mismatched model id over, and it
        // is unrecoverable because the attempt's evidence names the wrong CLI.
        for near in ["Dummy", "DUMMY", " dummy", "dummy ", "claude2", " ", ""] {
            let error = refusal(&configured(near));
            let Error::Config { key, detail } = &error else {
                panic!("`{near}` is not a provider, which is a configuration refusal, got {error}");
            };
            assert_eq!(
                key, "provider",
                "`{near}` is refused as the key that holds it"
            );
            assert!(
                detail.contains(&format!("`{near}`")),
                "the refusal quotes what was written, case and whitespace included, so \
                 the near miss is visible rather than repaired in silence: {detail}"
            );
        }
    }
}

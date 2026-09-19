//! The provider layer, and the one shape its reporting takes.
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

use serde::{Deserialize, Serialize};

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

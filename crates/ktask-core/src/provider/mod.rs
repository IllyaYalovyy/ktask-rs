//! The stable capability interface every provider adapter reports through,
//! per `VISION.md` §12 ("A stable capability interface, with adapters").
//!
//! [`Usage`] is the one shape token and cost reporting take anywhere in the
//! system: attempt evidence, the TUI, and any future export. A figure a
//! provider did not report is `None`, never a substituted `0` — a missing
//! cost and a free run are different facts, and collapsing them would make
//! attempt evidence lie. [`UsageSource`] records why a figure is present or
//! absent, so `Unavailable` is distinguishable from a value the provider
//! actually reported as zero.

use serde::{Deserialize, Serialize};

/// Token and cost usage for one attempt, as reported by (or recovered for) a
/// provider. Every field is independently optional: a provider may report
/// tokens but not cost, or cost but not cached tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input (prompt) tokens consumed, if known.
    pub input_tokens: Option<u64>,
    /// Output (completion) tokens produced, if known.
    pub output_tokens: Option<u64>,
    /// Tokens served from a prompt cache, if known and applicable.
    pub cached_tokens: Option<u64>,
    /// Cost in US dollars, if known.
    pub cost_usd: Option<f64>,
    /// Where these figures came from, or why they are absent.
    pub source: UsageSource,
}

/// Where a [`Usage`] figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSource {
    /// The provider reported usage directly, structured.
    Provider,
    /// No structured usage was available; figures were parsed out of the
    /// agent's textual output.
    ParsedFromOutput,
    /// No usage figures could be obtained at all.
    Unavailable,
}

#[cfg(test)]
mod usage {
    use super::*;

    fn all_usage_sources() -> Vec<UsageSource> {
        vec![
            UsageSource::Provider,
            UsageSource::ParsedFromOutput,
            UsageSource::Unavailable,
        ]
    }

    #[test]
    fn usage_source_has_exactly_three_variants() {
        let variants = all_usage_sources();
        assert_eq!(variants.len(), 3);

        // Exhaustive, wildcard-free match: a variant added to `UsageSource`
        // without being listed here fails to compile instead of silently
        // under-counting.
        for source in variants {
            match source {
                UsageSource::Provider
                | UsageSource::ParsedFromOutput
                | UsageSource::Unavailable => {}
            }
        }
    }

    #[test]
    fn every_usage_source_round_trips_through_json() {
        for source in all_usage_sources() {
            let json = serde_json::to_string(&source).expect("serialize");
            let back: UsageSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(source, back);
        }
    }

    #[test]
    fn usage_round_trips_through_json_with_every_figure_present() {
        let usage = Usage {
            input_tokens: Some(120),
            output_tokens: Some(45),
            cached_tokens: Some(30),
            cost_usd: Some(0.0123),
            source: UsageSource::Provider,
        };
        let json = serde_json::to_string(&usage).expect("serialize");
        let back: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(usage, back);
    }

    /// The behavior this type exists to guarantee: an unavailable figure is
    /// `None`, not a substituted `0`. A caller that matched on `Some(0)` to
    /// mean "unavailable" would be looking at the wrong field entirely, and
    /// the JSON on the wire makes that observable as `null`, not `0`.
    #[test]
    fn unavailable_usage_has_no_figures_and_serializes_them_as_null_not_zero() {
        let usage = Usage {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_usd: None,
            source: UsageSource::Unavailable,
        };

        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.source, UsageSource::Unavailable);

        let json = serde_json::to_value(usage).expect("serialize");
        assert_eq!(json["input_tokens"], serde_json::Value::Null);
        assert_eq!(json["output_tokens"], serde_json::Value::Null);
        assert_eq!(json["cached_tokens"], serde_json::Value::Null);
        assert_eq!(json["cost_usd"], serde_json::Value::Null);
        assert_ne!(json["input_tokens"], serde_json::json!(0));

        let back: Usage = serde_json::from_value(json).expect("deserialize");
        assert_eq!(usage, back);
    }
}

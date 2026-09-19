use serde::{Deserialize, Serialize};

/// The source of usage data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// Usage data came directly from the provider.
    Provider,
    /// Usage data was parsed from the provider's output.
    ParsedFromOutput,
    /// Usage data was not available from any source.
    Unavailable,
}

/// Token and cost reporting for a provider's response.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Usage {
    /// Number of input tokens used, if available.
    pub input_tokens: Option<u64>,
    /// Number of output tokens used, if available.
    pub output_tokens: Option<u64>,
    /// Number of cached tokens used, if available.
    pub cached_tokens: Option<u64>,
    /// Cost in USD, if available.
    pub cost_usd: Option<f64>,
    /// The source of this usage data.
    pub source: UsageSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_usage_source_roundtrips_through_json() {
        for source in [
            UsageSource::Provider,
            UsageSource::ParsedFromOutput,
            UsageSource::Unavailable,
        ] {
            let json = serde_json::to_string(&source).expect("serialize");
            let deserialized: UsageSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(source, deserialized);
        }
    }

    #[test]
    fn usage_with_all_fields_present() {
        let usage = Usage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cached_tokens: Some(10),
            cost_usd: Some(0.001),
            source: UsageSource::Provider,
        };
        let json = serde_json::to_string(&usage).expect("serialize");
        let deserialized: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(usage.input_tokens, deserialized.input_tokens);
        assert_eq!(usage.output_tokens, deserialized.output_tokens);
        assert_eq!(usage.cached_tokens, deserialized.cached_tokens);
        assert_eq!(usage.cost_usd, deserialized.cost_usd);
        assert_eq!(usage.source, deserialized.source);
    }

    #[test]
    fn usage_with_all_fields_none() {
        let usage = Usage {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_usd: None,
            source: UsageSource::Unavailable,
        };
        let json = serde_json::to_string(&usage).expect("serialize");
        let deserialized: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(usage.input_tokens, deserialized.input_tokens);
        assert_eq!(usage.output_tokens, deserialized.output_tokens);
        assert_eq!(usage.cached_tokens, deserialized.cached_tokens);
        assert_eq!(usage.cost_usd, deserialized.cost_usd);
        assert_eq!(usage.source, deserialized.source);
    }

    #[test]
    fn usage_with_partial_fields() {
        let usage = Usage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cached_tokens: None,
            cost_usd: None,
            source: UsageSource::ParsedFromOutput,
        };
        let json = serde_json::to_string(&usage).expect("serialize");
        let deserialized: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(usage.input_tokens, deserialized.input_tokens);
        assert_eq!(usage.output_tokens, deserialized.output_tokens);
        assert_eq!(usage.cached_tokens, deserialized.cached_tokens);
        assert_eq!(usage.cost_usd, deserialized.cost_usd);
        assert_eq!(usage.source, deserialized.source);
    }

    #[test]
    fn unavailable_figures_are_none_with_unavailable_source() {
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
    }

    #[test]
    fn usage_roundtrips_through_json_with_all_sources() {
        for source in [
            UsageSource::Provider,
            UsageSource::ParsedFromOutput,
            UsageSource::Unavailable,
        ] {
            let usage = Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                cached_tokens: Some(10),
                cost_usd: Some(0.001),
                source,
            };
            let json = serde_json::to_string(&usage).expect("serialize");
            let deserialized: Usage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(usage.source, deserialized.source);
        }
    }
}

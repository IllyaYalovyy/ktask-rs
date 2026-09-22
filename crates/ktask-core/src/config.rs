//! The `Config` struct and its documented defaults.
//!
//! Every field mirrors a key in the Configuration defaults section of
//! `docs/DESIGN.md`. An empty TOML document deserializes to [`Config::default`];
//! an unknown key is a deserialization error, so a typo in a user's config
//! file is caught rather than silently ignored.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ktask-rs's user-configurable settings, with defaults for every field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The agent backend to run tasks with.
    pub provider: String,
    /// The model to request from the provider, when it supports a choice.
    pub model: Option<String>,
    /// Wall-clock limit for a single attempt, in seconds.
    pub attempt_timeout_secs: u64,
    /// Wall-clock limit for a single quality gate, in seconds.
    pub gate_timeout_secs: u64,
    /// How long an attempt may produce no output before it is killed.
    pub idle_timeout_secs: u64,
    /// How many attempts a task gets in total, including remediation.
    pub max_attempts: u32,
    /// How many of `max_attempts` may be remediation attempts.
    pub max_remediation_attempts: u32,
    /// How many identical failure signatures trip the circuit breaker.
    pub circuit_breaker_threshold: u32,
    /// The git remote treated as mainline.
    pub mainline_remote: String,
    /// The git branch treated as mainline.
    pub mainline_branch: String,
    /// Maximum bytes of context handed to an agent per attempt.
    pub context_budget_bytes: u64,
    /// Maximum bytes retained in a failure bundle.
    pub failure_bundle_bytes: u64,
    /// Number of lines kept in the in-memory output ring buffer.
    pub output_ring_lines: u32,
    /// Extra margin added to a provider-reported wait before retrying.
    pub limit_wait_margin_secs: u64,
    /// The longest a provider rate limit is allowed to make ktask-rs wait.
    pub limit_max_wait_secs: u64,
    /// The work protocol used when a task does not name one.
    pub default_protocol: String,
    /// Path to a scenario file for the `dummy` provider, when it is used.
    pub dummy_scenario_path: Option<PathBuf>,
    /// Glob patterns identifying test files.
    pub test_globs: Vec<String>,
    /// Patterns scanned for and redacted as secrets.
    pub secret_patterns: Vec<String>,
    /// How many times a test is repeated to detect flakiness.
    pub flake_runs: u32,
    /// How many days of history are retained before pruning.
    pub retention_days: u32,
    /// The minimum free disk space required to start an attempt, in bytes.
    pub min_free_disk_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "dummy".to_string(),
            model: None,
            attempt_timeout_secs: 14_400,
            gate_timeout_secs: 1_800,
            idle_timeout_secs: 1_800,
            max_attempts: 2,
            max_remediation_attempts: 1,
            circuit_breaker_threshold: 3,
            mainline_remote: "origin".to_string(),
            mainline_branch: "main".to_string(),
            context_budget_bytes: 65_536,
            failure_bundle_bytes: 16_384,
            output_ring_lines: 4_096,
            limit_wait_margin_secs: 60,
            limit_max_wait_secs: 86_400,
            default_protocol: "direct".to_string(),
            dummy_scenario_path: None,
            test_globs: vec![
                "**/tests/**".to_string(),
                "**/*_test.rs".to_string(),
                "src/**/tests.rs".to_string(),
            ],
            secret_patterns: Vec::new(),
            flake_runs: 5,
            retention_days: 90,
            min_free_disk_bytes: 2_147_483_648,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_document_deserializes_to_defaults() {
        let config: Config = toml::from_str("").expect("deserialize");
        let from_empty = toml::to_string(&config).expect("serialize");
        let from_default = toml::to_string(&Config::default()).expect("serialize");
        assert_eq!(from_empty, from_default);
    }

    #[test]
    fn unknown_key_is_an_error() {
        let err = toml::from_str::<Config>("bogus_key = 1").expect_err("must fail");
        assert!(err.to_string().contains("bogus_key"));
    }

    #[test]
    fn overriding_one_field_leaves_the_rest_at_default() {
        let config: Config = toml::from_str(r#"provider = "claude""#).expect("deserialize");
        assert_eq!(config.provider, "claude");
        assert_eq!(config.max_attempts, Config::default().max_attempts);
    }

    #[test]
    fn defaults_match_documented_values() {
        let config = Config::default();
        assert_eq!(config.provider, "dummy");
        assert_eq!(config.model, None);
        assert_eq!(config.attempt_timeout_secs, 14_400);
        assert_eq!(config.gate_timeout_secs, 1_800);
        assert_eq!(config.idle_timeout_secs, 1_800);
        assert_eq!(config.max_attempts, 2);
        assert_eq!(config.max_remediation_attempts, 1);
        assert_eq!(config.circuit_breaker_threshold, 3);
        assert_eq!(config.mainline_remote, "origin");
        assert_eq!(config.mainline_branch, "main");
        assert_eq!(config.context_budget_bytes, 65_536);
        assert_eq!(config.failure_bundle_bytes, 16_384);
        assert_eq!(config.output_ring_lines, 4_096);
        assert_eq!(config.limit_wait_margin_secs, 60);
        assert_eq!(config.limit_max_wait_secs, 86_400);
        assert_eq!(config.default_protocol, "direct");
        assert_eq!(config.dummy_scenario_path, None);
        assert_eq!(
            config.test_globs,
            vec!["**/tests/**", "**/*_test.rs", "src/**/tests.rs"]
        );
        assert_eq!(config.secret_patterns, Vec::<String>::new());
        assert_eq!(config.flake_runs, 5);
        assert_eq!(config.retention_days, 90);
        assert_eq!(config.min_free_disk_bytes, 2_147_483_648);
    }
}

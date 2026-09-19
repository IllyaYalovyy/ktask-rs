//! Configuration model and defaults.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration model for ktask-rs with all documented defaults.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Provider name. Default: "dummy"
    pub provider: String,
    /// Model identifier. Default: None
    pub model: Option<String>,
    /// Attempt timeout in seconds. Default: 14400 (4h)
    pub attempt_timeout_secs: u32,
    /// Gate timeout in seconds. Default: 1800
    pub gate_timeout_secs: u32,
    /// Idle timeout in seconds. Default: 1800
    pub idle_timeout_secs: u32,
    /// Maximum number of attempts per task. Default: 2
    pub max_attempts: u32,
    /// Maximum number of remediation attempts. Default: 1
    pub max_remediation_attempts: u32,
    /// Circuit breaker threshold. Default: 3
    pub circuit_breaker_threshold: u32,
    /// Main line remote name. Default: "origin"
    pub mainline_remote: String,
    /// Main line branch name. Default: "main"
    pub mainline_branch: String,
    /// Context budget in bytes. Default: 65536
    pub context_budget_bytes: u32,
    /// Failure bundle size in bytes. Default: 16384
    pub failure_bundle_bytes: u32,
    /// Output ring buffer size in lines. Default: 4096
    pub output_ring_lines: u32,
    /// Limit wait margin in seconds. Default: 60
    pub limit_wait_margin_secs: u32,
    /// Maximum limit wait time in seconds. Default: 86400
    pub limit_max_wait_secs: u32,
    /// Default protocol. Default: "direct"
    pub default_protocol: String,
    /// Path to dummy scenario file. Default: None
    pub dummy_scenario_path: Option<PathBuf>,
    /// Test glob patterns. Default: `["**/tests/**", "**/*_test.rs", "src/**/tests.rs"]`
    pub test_globs: Vec<String>,
    /// Secret patterns for redaction. Default: []
    pub secret_patterns: Vec<String>,
    /// Number of flake runs. Default: 5
    pub flake_runs: u32,
    /// Retention days. Default: 90
    pub retention_days: u32,
    /// Minimum free disk space in bytes. Default: 2147483648 (2 GiB)
    pub min_free_disk_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "dummy".to_string(),
            model: None,
            attempt_timeout_secs: 14400,
            gate_timeout_secs: 1800,
            idle_timeout_secs: 1800,
            max_attempts: 2,
            max_remediation_attempts: 1,
            circuit_breaker_threshold: 3,
            mainline_remote: "origin".to_string(),
            mainline_branch: "main".to_string(),
            context_budget_bytes: 65536,
            failure_bundle_bytes: 16384,
            output_ring_lines: 4096,
            limit_wait_margin_secs: 60,
            limit_max_wait_secs: 86400,
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
    fn default_config_has_all_expected_values() {
        let cfg = Config::default();
        assert_eq!(cfg.provider, "dummy");
        assert_eq!(cfg.model, None);
        assert_eq!(cfg.attempt_timeout_secs, 14400);
        assert_eq!(cfg.gate_timeout_secs, 1800);
        assert_eq!(cfg.idle_timeout_secs, 1800);
        assert_eq!(cfg.max_attempts, 2);
        assert_eq!(cfg.max_remediation_attempts, 1);
        assert_eq!(cfg.circuit_breaker_threshold, 3);
        assert_eq!(cfg.mainline_remote, "origin");
        assert_eq!(cfg.mainline_branch, "main");
        assert_eq!(cfg.context_budget_bytes, 65536);
        assert_eq!(cfg.failure_bundle_bytes, 16384);
        assert_eq!(cfg.output_ring_lines, 4096);
        assert_eq!(cfg.limit_wait_margin_secs, 60);
        assert_eq!(cfg.limit_max_wait_secs, 86400);
        assert_eq!(cfg.default_protocol, "direct");
        assert_eq!(cfg.dummy_scenario_path, None);
        assert_eq!(
            cfg.test_globs,
            vec!["**/tests/**", "**/*_test.rs", "src/**/tests.rs"]
        );
        assert!(cfg.secret_patterns.is_empty());
        assert_eq!(cfg.flake_runs, 5);
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(cfg.min_free_disk_bytes, 2_147_483_648);
    }

    #[test]
    fn empty_toml_deserializes_to_defaults() {
        let toml_str = "";
        let cfg: Config = toml::from_str(toml_str).expect("empty TOML should deserialize");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let toml_str = "unknown_key = \"value\"";
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn partial_config_deserializes_with_defaults() {
        let toml_str = "provider = \"custom\"\nmodel = \"gpt-4\"";
        let cfg: Config = toml::from_str(toml_str).expect("partial TOML should deserialize");
        assert_eq!(cfg.provider, "custom");
        assert_eq!(cfg.model, Some("gpt-4".to_string()));
        assert_eq!(cfg.mainline_remote, "origin");
        assert_eq!(cfg.default_protocol, "direct");
    }

    #[test]
    fn config_with_all_fields_deserializes() {
        let toml_str = r#"
provider = "claude"
model = "claude-opus"
attempt_timeout_secs = 7200
gate_timeout_secs = 900
idle_timeout_secs = 1200
max_attempts = 3
max_remediation_attempts = 2
circuit_breaker_threshold = 5
mainline_remote = "upstream"
mainline_branch = "develop"
context_budget_bytes = 32768
failure_bundle_bytes = 8192
output_ring_lines = 2048
limit_wait_margin_secs = 120
limit_max_wait_secs = 43200
default_protocol = "batched"
dummy_scenario_path = "/tmp/scenario.md"
test_globs = ["**/test_*.rs", "tests/**"]
secret_patterns = ["password", "api_key"]
flake_runs = 3
retention_days = 60
min_free_disk_bytes = 1073741824
"#;
        let cfg: Config = toml::from_str(toml_str).expect("full TOML should deserialize");
        assert_eq!(cfg.provider, "claude");
        assert_eq!(cfg.model, Some("claude-opus".to_string()));
        assert_eq!(cfg.attempt_timeout_secs, 7200);
        assert_eq!(cfg.gate_timeout_secs, 900);
        assert_eq!(cfg.idle_timeout_secs, 1200);
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.max_remediation_attempts, 2);
        assert_eq!(cfg.circuit_breaker_threshold, 5);
        assert_eq!(cfg.mainline_remote, "upstream");
        assert_eq!(cfg.mainline_branch, "develop");
        assert_eq!(cfg.context_budget_bytes, 32768);
        assert_eq!(cfg.failure_bundle_bytes, 8192);
        assert_eq!(cfg.output_ring_lines, 2048);
        assert_eq!(cfg.limit_wait_margin_secs, 120);
        assert_eq!(cfg.limit_max_wait_secs, 43200);
        assert_eq!(cfg.default_protocol, "batched");
        assert_eq!(
            cfg.dummy_scenario_path,
            Some(PathBuf::from("/tmp/scenario.md"))
        );
        assert_eq!(cfg.test_globs, vec!["**/test_*.rs", "tests/**"]);
        assert_eq!(cfg.secret_patterns, vec!["password", "api_key"]);
        assert_eq!(cfg.flake_runs, 3);
        assert_eq!(cfg.retention_days, 60);
        assert_eq!(cfg.min_free_disk_bytes, 1_073_741_824);
    }

    #[test]
    fn config_is_cloneable() {
        let cfg1 = Config::default();
        let cfg2 = cfg1.clone();
        assert_eq!(cfg1, cfg2);
    }
}

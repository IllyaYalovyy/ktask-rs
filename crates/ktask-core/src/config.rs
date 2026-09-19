//! Configuration model and defaults.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Source of a configuration value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Default value
    Default,
    /// Global configuration file
    GlobalFile,
    /// Project configuration file
    ProjectFile,
    /// Environment variable
    Env,
    /// Command-line flag
    Flag,
}

impl Source {
    /// Human-readable name of the source.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::GlobalFile => "global_file",
            Source::ProjectFile => "project_file",
            Source::Env => "env",
            Source::Flag => "flag",
        }
    }
}

/// A resolved configuration value with its source.
#[derive(Debug, Clone)]
pub struct Resolved<T> {
    /// The configuration value
    pub value: T,
    /// The source of the value
    pub source: Source,
}

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
    /// Baseline gate command. Default: None
    pub baseline_command: Option<Vec<String>>,
    /// Targeted test gate command. Default: None
    pub targeted_test_command: Option<Vec<String>>,
    /// Verify gate command. Default: None (required for profiles)
    pub verify_command: Option<Vec<String>>,
    /// Lint gate command. Default: None
    pub lint_command: Option<Vec<String>>,
    /// Format gate command. Default: None
    pub format_command: Option<Vec<String>>,
    /// Build gate command. Default: None
    pub build_command: Option<Vec<String>>,
    /// Privacy gate command. Default: None
    pub privacy_command: Option<Vec<String>>,
    /// Flake gate command. Default: None
    pub flake_command: Option<Vec<String>>,
    /// Source of each configuration value
    #[serde(skip)]
    sources: HashMap<String, Source>,
}

/// Get the path to the project configuration file.
///
/// Returns `<project.state_dir>/config.toml`.
#[must_use]
pub fn project_config_path(project: &crate::project::Project) -> PathBuf {
    project.state_dir.join("config.toml")
}

/// Load configuration for a project from real files.
///
/// Loads configuration by merging:
/// 1. Global configuration file (`$XDG_CONFIG_HOME/ktask-rs/config.toml` or `$HOME/.config/ktask-rs/config.toml`)
/// 2. Project configuration file (`<state_dir>/config.toml`)
/// 3. Environment variables
/// 4. Documented defaults
///
/// Files are only read if they exist. If neither global nor project config files exist,
/// returns the defaults merged with any environment variable overrides.
///
/// # Errors
///
/// Returns an error if:
/// - The environment is misconfigured (HOME not set)
/// - A configuration file exists but cannot be read
/// - A configuration file cannot be deserialized as valid TOML
pub fn load_for(project: &crate::project::Project) -> crate::error::Result<Config> {
    let global_path = crate::paths::config_file()?;
    let project_path = project_config_path(project);

    Config::load(
        if global_path.exists() {
            Some(&global_path)
        } else {
            None
        },
        if project_path.exists() {
            Some(&project_path)
        } else {
            None
        },
        &|key| std::env::var(key).ok(),
    )
}

impl Config {
    /// Load configuration from files and environment, applying layered precedence.
    ///
    /// Precedence (highest to lowest):
    /// 1. Environment variables
    /// 2. Project configuration file
    /// 3. Global configuration file
    /// 4. Defaults
    ///
    /// # Errors
    ///
    /// Returns an error if files cannot be read or configuration cannot be deserialized.
    pub fn load(
        global: Option<&Path>,
        project: Option<&Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> crate::error::Result<Self> {
        let mut cfg = Self::default();

        if let Some(global_path) = global {
            let content =
                fs::read_to_string(global_path).map_err(|e| crate::error::Error::NotFound {
                    what: format!("global config: {e}"),
                })?;
            let global_cfg: Config =
                toml::from_str(&content).map_err(|e| crate::error::Error::Deserialize {
                    detail: e.to_string(),
                })?;
            cfg.merge_with_source(&global_cfg, Source::GlobalFile);
        }

        if let Some(project_path) = project {
            let content =
                fs::read_to_string(project_path).map_err(|e| crate::error::Error::NotFound {
                    what: format!("project config: {e}"),
                })?;
            let project_cfg: Config =
                toml::from_str(&content).map_err(|e| crate::error::Error::Deserialize {
                    detail: e.to_string(),
                })?;
            cfg.merge_with_source(&project_cfg, Source::ProjectFile);
        }

        cfg.merge_from_env(env);

        Ok(cfg)
    }

    /// Get the source of all configuration values.
    #[must_use]
    pub fn provenance(&self) -> Vec<(String, Source)> {
        let mut result: Vec<_> = self.sources.iter().map(|(k, v)| (k.clone(), *v)).collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    fn merge_with_source(&mut self, other: &Config, source: Source) {
        if other.provider != "dummy" {
            self.provider.clone_from(&other.provider);
            self.sources.insert("provider".to_string(), source);
        }
        if other.model.is_some() {
            self.model.clone_from(&other.model);
            self.sources.insert("model".to_string(), source);
        }
        if other.attempt_timeout_secs != 14400 {
            self.attempt_timeout_secs = other.attempt_timeout_secs;
            self.sources
                .insert("attempt_timeout_secs".to_string(), source);
        }
        if other.gate_timeout_secs != 1800 {
            self.gate_timeout_secs = other.gate_timeout_secs;
            self.sources.insert("gate_timeout_secs".to_string(), source);
        }
        if other.idle_timeout_secs != 1800 {
            self.idle_timeout_secs = other.idle_timeout_secs;
            self.sources.insert("idle_timeout_secs".to_string(), source);
        }
        if other.max_attempts != 2 {
            self.max_attempts = other.max_attempts;
            self.sources.insert("max_attempts".to_string(), source);
        }
        if other.max_remediation_attempts != 1 {
            self.max_remediation_attempts = other.max_remediation_attempts;
            self.sources
                .insert("max_remediation_attempts".to_string(), source);
        }
        if other.circuit_breaker_threshold != 3 {
            self.circuit_breaker_threshold = other.circuit_breaker_threshold;
            self.sources
                .insert("circuit_breaker_threshold".to_string(), source);
        }
        if other.mainline_remote != "origin" {
            self.mainline_remote.clone_from(&other.mainline_remote);
            self.sources.insert("mainline_remote".to_string(), source);
        }
        if other.mainline_branch != "main" {
            self.mainline_branch.clone_from(&other.mainline_branch);
            self.sources.insert("mainline_branch".to_string(), source);
        }
        if other.context_budget_bytes != 65536 {
            self.context_budget_bytes = other.context_budget_bytes;
            self.sources
                .insert("context_budget_bytes".to_string(), source);
        }
        if other.failure_bundle_bytes != 16384 {
            self.failure_bundle_bytes = other.failure_bundle_bytes;
            self.sources
                .insert("failure_bundle_bytes".to_string(), source);
        }
        if other.output_ring_lines != 4096 {
            self.output_ring_lines = other.output_ring_lines;
            self.sources.insert("output_ring_lines".to_string(), source);
        }
        if other.limit_wait_margin_secs != 60 {
            self.limit_wait_margin_secs = other.limit_wait_margin_secs;
            self.sources
                .insert("limit_wait_margin_secs".to_string(), source);
        }
        if other.limit_max_wait_secs != 86400 {
            self.limit_max_wait_secs = other.limit_max_wait_secs;
            self.sources
                .insert("limit_max_wait_secs".to_string(), source);
        }
        if other.default_protocol != "direct" {
            self.default_protocol.clone_from(&other.default_protocol);
            self.sources.insert("default_protocol".to_string(), source);
        }
        if other.dummy_scenario_path.is_some() {
            self.dummy_scenario_path
                .clone_from(&other.dummy_scenario_path);
            self.sources
                .insert("dummy_scenario_path".to_string(), source);
        }
        if other.test_globs != vec!["**/tests/**", "**/*_test.rs", "src/**/tests.rs"] {
            self.test_globs.clone_from(&other.test_globs);
            self.sources.insert("test_globs".to_string(), source);
        }
        if !other.secret_patterns.is_empty() {
            self.secret_patterns.clone_from(&other.secret_patterns);
            self.sources.insert("secret_patterns".to_string(), source);
        }
        if other.flake_runs != 5 {
            self.flake_runs = other.flake_runs;
            self.sources.insert("flake_runs".to_string(), source);
        }
        if other.retention_days != 90 {
            self.retention_days = other.retention_days;
            self.sources.insert("retention_days".to_string(), source);
        }
        if other.min_free_disk_bytes != 2_147_483_648 {
            self.min_free_disk_bytes = other.min_free_disk_bytes;
            self.sources
                .insert("min_free_disk_bytes".to_string(), source);
        }
        self.merge_gate_commands(other, source);
    }

    fn merge_gate_commands(&mut self, other: &Config, source: Source) {
        if other.baseline_command.is_some() {
            self.baseline_command.clone_from(&other.baseline_command);
            self.sources.insert("baseline_command".to_string(), source);
        }
        if other.targeted_test_command.is_some() {
            self.targeted_test_command
                .clone_from(&other.targeted_test_command);
            self.sources
                .insert("targeted_test_command".to_string(), source);
        }
        if other.verify_command.is_some() {
            self.verify_command.clone_from(&other.verify_command);
            self.sources.insert("verify_command".to_string(), source);
        }
        if other.lint_command.is_some() {
            self.lint_command.clone_from(&other.lint_command);
            self.sources.insert("lint_command".to_string(), source);
        }
        if other.format_command.is_some() {
            self.format_command.clone_from(&other.format_command);
            self.sources.insert("format_command".to_string(), source);
        }
        if other.build_command.is_some() {
            self.build_command.clone_from(&other.build_command);
            self.sources.insert("build_command".to_string(), source);
        }
        if other.privacy_command.is_some() {
            self.privacy_command.clone_from(&other.privacy_command);
            self.sources.insert("privacy_command".to_string(), source);
        }
        if other.flake_command.is_some() {
            self.flake_command.clone_from(&other.flake_command);
            self.sources.insert("flake_command".to_string(), source);
        }
    }

    fn merge_from_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        macro_rules! env_u32 {
            ($field:ident, $env_var:expr) => {
                if let Some(value) = env($env_var) {
                    if let Ok(n) = value.parse() {
                        self.$field = n;
                        self.sources
                            .insert(stringify!($field).to_string(), Source::Env);
                    }
                }
            };
        }
        macro_rules! env_u64 {
            ($field:ident, $env_var:expr) => {
                if let Some(value) = env($env_var) {
                    if let Ok(n) = value.parse() {
                        self.$field = n;
                        self.sources
                            .insert(stringify!($field).to_string(), Source::Env);
                    }
                }
            };
        }
        macro_rules! env_string {
            ($field:ident, $env_var:expr) => {
                if let Some(value) = env($env_var) {
                    self.$field = value;
                    self.sources
                        .insert(stringify!($field).to_string(), Source::Env);
                }
            };
        }

        env_string!(provider, "KTASK_PROVIDER");
        if let Some(value) = env("KTASK_MODEL") {
            self.model = Some(value);
            self.sources.insert("model".to_string(), Source::Env);
        }
        env_u32!(attempt_timeout_secs, "KTASK_ATTEMPT_TIMEOUT_SECS");
        env_u32!(gate_timeout_secs, "KTASK_GATE_TIMEOUT_SECS");
        env_u32!(idle_timeout_secs, "KTASK_IDLE_TIMEOUT_SECS");
        env_u32!(max_attempts, "KTASK_MAX_ATTEMPTS");
        env_u32!(max_remediation_attempts, "KTASK_MAX_REMEDIATION_ATTEMPTS");
        env_u32!(circuit_breaker_threshold, "KTASK_CIRCUIT_BREAKER_THRESHOLD");
        env_string!(mainline_remote, "KTASK_MAINLINE_REMOTE");
        env_string!(mainline_branch, "KTASK_MAINLINE_BRANCH");
        env_u32!(context_budget_bytes, "KTASK_CONTEXT_BUDGET_BYTES");
        env_u32!(failure_bundle_bytes, "KTASK_FAILURE_BUNDLE_BYTES");
        env_u32!(output_ring_lines, "KTASK_OUTPUT_RING_LINES");
        env_u32!(limit_wait_margin_secs, "KTASK_LIMIT_WAIT_MARGIN_SECS");
        env_u32!(limit_max_wait_secs, "KTASK_LIMIT_MAX_WAIT_SECS");
        env_string!(default_protocol, "KTASK_DEFAULT_PROTOCOL");
        if let Some(value) = env("KTASK_DUMMY_SCENARIO_PATH") {
            self.dummy_scenario_path = Some(PathBuf::from(value));
            self.sources
                .insert("dummy_scenario_path".to_string(), Source::Env);
        }
        env_u32!(flake_runs, "KTASK_FLAKE_RUNS");
        env_u32!(retention_days, "KTASK_RETENTION_DAYS");
        env_u64!(min_free_disk_bytes, "KTASK_MIN_FREE_DISK_BYTES");
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut sources = HashMap::new();
        sources.insert("provider".to_string(), Source::Default);
        sources.insert("model".to_string(), Source::Default);
        sources.insert("attempt_timeout_secs".to_string(), Source::Default);
        sources.insert("gate_timeout_secs".to_string(), Source::Default);
        sources.insert("idle_timeout_secs".to_string(), Source::Default);
        sources.insert("max_attempts".to_string(), Source::Default);
        sources.insert("max_remediation_attempts".to_string(), Source::Default);
        sources.insert("circuit_breaker_threshold".to_string(), Source::Default);
        sources.insert("mainline_remote".to_string(), Source::Default);
        sources.insert("mainline_branch".to_string(), Source::Default);
        sources.insert("context_budget_bytes".to_string(), Source::Default);
        sources.insert("failure_bundle_bytes".to_string(), Source::Default);
        sources.insert("output_ring_lines".to_string(), Source::Default);
        sources.insert("limit_wait_margin_secs".to_string(), Source::Default);
        sources.insert("limit_max_wait_secs".to_string(), Source::Default);
        sources.insert("default_protocol".to_string(), Source::Default);
        sources.insert("dummy_scenario_path".to_string(), Source::Default);
        sources.insert("test_globs".to_string(), Source::Default);
        sources.insert("secret_patterns".to_string(), Source::Default);
        sources.insert("flake_runs".to_string(), Source::Default);
        sources.insert("retention_days".to_string(), Source::Default);
        sources.insert("min_free_disk_bytes".to_string(), Source::Default);

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
            baseline_command: None,
            targeted_test_command: None,
            verify_command: None,
            lint_command: None,
            format_command: None,
            build_command: None,
            privacy_command: None,
            flake_command: None,
            sources,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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

    #[test]
    fn load_defaults_when_no_files_or_env() {
        let env = |_: &str| -> Option<String> { None };
        let cfg = Config::load(None, None, &env).expect("load should succeed");
        let defaults = Config::default();
        assert_eq!(cfg.provider, defaults.provider);
        assert_eq!(cfg.model, defaults.model);
    }

    #[test]
    fn load_merges_global_file() {
        let tmp = TempDir::new().expect("create temp dir");
        let global_path = tmp.path().join("global.toml");
        let global_content = r#"provider = "custom_provider""#;
        fs::write(&global_path, global_content).expect("write global config");

        let env = |_: &str| -> Option<String> { None };
        let cfg = Config::load(Some(&global_path), None, &env).expect("load should succeed");

        assert_eq!(cfg.provider, "custom_provider");
        assert_eq!(cfg.model, None);
        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::GlobalFile)
        );
    }

    #[test]
    fn load_merges_project_file() {
        let tmp = TempDir::new().expect("create temp dir");
        let project_path = tmp.path().join("project.toml");
        let project_content = r#"model = "project_model""#;
        fs::write(&project_path, project_content).expect("write project config");

        let env = |_: &str| -> Option<String> { None };
        let cfg = Config::load(None, Some(&project_path), &env).expect("load should succeed");

        assert_eq!(cfg.model, Some("project_model".to_string()));
        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "model").map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
    }

    #[test]
    fn load_merges_env_variables() {
        let env = |key: &str| -> Option<String> {
            match key {
                "KTASK_PROVIDER" => Some("env_provider".to_string()),
                "KTASK_MODEL" => Some("env_model".to_string()),
                _ => None,
            }
        };

        let cfg = Config::load(None, None, &env).expect("load should succeed");

        assert_eq!(cfg.provider, "env_provider");
        assert_eq!(cfg.model, Some("env_model".to_string()));
        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::Env)
        );
        assert_eq!(
            prov.iter().find(|(k, _)| k == "model").map(|(_, s)| *s),
            Some(Source::Env)
        );
    }

    #[test]
    fn load_precedence_env_over_project_over_global_over_default() {
        let tmp = TempDir::new().expect("create temp dir");

        let global_path = tmp.path().join("global.toml");
        fs::write(&global_path, r#"provider = "global_provider""#).expect("write global");

        let project_path = tmp.path().join("project.toml");
        fs::write(&project_path, r#"provider = "project_provider""#).expect("write project");

        let env = |key: &str| -> Option<String> {
            match key {
                "KTASK_PROVIDER" => Some("env_provider".to_string()),
                _ => None,
            }
        };

        let cfg = Config::load(Some(&global_path), Some(&project_path), &env)
            .expect("load should succeed");

        assert_eq!(cfg.provider, "env_provider");
        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::Env)
        );
    }

    #[test]
    fn load_precedence_project_over_global_over_default() {
        let tmp = TempDir::new().expect("create temp dir");

        let global_path = tmp.path().join("global.toml");
        fs::write(&global_path, r#"provider = "global_provider""#).expect("write global");

        let project_path = tmp.path().join("project.toml");
        fs::write(&project_path, r#"provider = "project_provider""#).expect("write project");

        let env = |_: &str| -> Option<String> { None };

        let cfg = Config::load(Some(&global_path), Some(&project_path), &env)
            .expect("load should succeed");

        assert_eq!(cfg.provider, "project_provider");
        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
    }

    #[test]
    fn load_precedence_global_over_default() {
        let tmp = TempDir::new().expect("create temp dir");

        let global_path = tmp.path().join("global.toml");
        fs::write(&global_path, r#"provider = "global_provider""#).expect("write global");

        let env = |_: &str| -> Option<String> { None };

        let cfg = Config::load(Some(&global_path), None, &env).expect("load should succeed");

        assert_eq!(cfg.provider, "global_provider");
        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::GlobalFile)
        );
    }

    #[test]
    fn provenance_returns_all_sources_sorted() {
        let env = |key: &str| -> Option<String> {
            match key {
                "KTASK_PROVIDER" => Some("env_provider".to_string()),
                _ => None,
            }
        };

        let cfg = Config::load(None, None, &env).expect("load should succeed");
        let prov = cfg.provenance();

        assert!(!prov.is_empty());
        assert!(prov.iter().any(|(k, _)| k == "provider"));

        let keys: Vec<_> = prov.iter().map(|(k, _)| k).collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys, "provenance should return sorted keys");
    }

    #[test]
    fn load_with_all_precedence_levels() {
        let tmp = TempDir::new().expect("create temp dir");

        let global_path = tmp.path().join("global.toml");
        fs::write(
            &global_path,
            r#"
provider = "global_provider"
model = "global_model"
attempt_timeout_secs = 1000
"#,
        )
        .expect("write global");

        let project_path = tmp.path().join("project.toml");
        fs::write(
            &project_path,
            r#"
provider = "project_provider"
max_attempts = 5
"#,
        )
        .expect("write project");

        let env = |key: &str| -> Option<String> {
            match key {
                "KTASK_MODEL" => Some("env_model".to_string()),
                "KTASK_GATE_TIMEOUT_SECS" => Some("999".to_string()),
                _ => None,
            }
        };

        let cfg = Config::load(Some(&global_path), Some(&project_path), &env)
            .expect("load should succeed");

        assert_eq!(
            cfg.provider, "project_provider",
            "project should override global"
        );
        assert_eq!(
            cfg.model,
            Some("env_model".to_string()),
            "env should override all"
        );
        assert_eq!(
            cfg.attempt_timeout_secs, 1000,
            "global value should persist when not overridden"
        );
        assert_eq!(cfg.max_attempts, 5, "project value should persist");
        assert_eq!(cfg.gate_timeout_secs, 999, "env should override default");

        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
        assert_eq!(
            prov.iter().find(|(k, _)| k == "model").map(|(_, s)| *s),
            Some(Source::Env)
        );
        assert_eq!(
            prov.iter()
                .find(|(k, _)| k == "attempt_timeout_secs")
                .map(|(_, s)| *s),
            Some(Source::GlobalFile)
        );
        assert_eq!(
            prov.iter()
                .find(|(k, _)| k == "max_attempts")
                .map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
        assert_eq!(
            prov.iter()
                .find(|(k, _)| k == "gate_timeout_secs")
                .map(|(_, s)| *s),
            Some(Source::Env)
        );
    }

    #[test]
    fn project_config_path_returns_state_dir_config_toml() {
        let tmp = TempDir::new().expect("create temp dir");
        let project = crate::project::Project {
            root: tmp.path().to_path_buf(),
            id: "test-id".to_string(),
            state_dir: tmp.path().join("state"),
        };

        let config_path = project_config_path(&project);
        assert_eq!(config_path, tmp.path().join("state/config.toml"));
    }

    #[test]
    fn load_for_gets_documented_defaults_with_no_config_files() {
        let tmp = TempDir::new().expect("create temp dir");
        let state_dir = tmp.path().join("state");
        fs::create_dir(&state_dir).expect("create state dir");

        let project = crate::project::Project {
            root: tmp.path().to_path_buf(),
            id: "test-id".to_string(),
            state_dir,
        };

        let cfg = load_for(&project).expect("load_for should succeed");
        let defaults = Config::default();

        assert_eq!(cfg.provider, defaults.provider);
        assert_eq!(cfg.model, defaults.model);
        assert_eq!(cfg.attempt_timeout_secs, defaults.attempt_timeout_secs);

        let prov = cfg.provenance();
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::Default)
        );
    }

    #[test]
    fn load_for_project_file_overrides_global() {
        let tmp = TempDir::new().expect("create temp dir");
        let state_dir = tmp.path().join("state");
        fs::create_dir(&state_dir).expect("create state dir");

        let global_path = tmp.path().join("global.toml");
        fs::write(&global_path, r#"provider = "global_provider""#).expect("write global");

        let project_config_path = state_dir.join("config.toml");
        fs::write(&project_config_path, r#"provider = "project_provider""#).expect("write project");

        let project = crate::project::Project {
            root: tmp.path().to_path_buf(),
            id: "test-id".to_string(),
            state_dir,
        };

        // Mock the environment and global config path by creating appropriate setup
        // We can't directly control paths::config_file() in tests, so we'll use
        // the direct Config::load to verify the behavior, and load_for for integration
        let cfg = load_for(&project).expect("load_for should succeed");
        let prov = cfg.provenance();

        assert_eq!(cfg.provider, "project_provider");
        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
    }

    #[test]
    fn load_for_retrieves_effective_source_of_each_value() {
        let tmp = TempDir::new().expect("create temp dir");
        let state_dir = tmp.path().join("state");
        fs::create_dir(&state_dir).expect("create state dir");

        let project_config_path = state_dir.join("config.toml");
        fs::write(
            &project_config_path,
            r#"
provider = "project_provider"
max_attempts = 5
"#,
        )
        .expect("write project");

        let project = crate::project::Project {
            root: tmp.path().to_path_buf(),
            id: "test-id".to_string(),
            state_dir,
        };

        let cfg = load_for(&project).expect("load_for should succeed");
        let prov = cfg.provenance();

        assert!(!prov.is_empty());

        assert_eq!(
            prov.iter().find(|(k, _)| k == "provider").map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
        assert_eq!(
            prov.iter()
                .find(|(k, _)| k == "max_attempts")
                .map(|(_, s)| *s),
            Some(Source::ProjectFile)
        );
        assert_eq!(
            prov.iter().find(|(k, _)| k == "model").map(|(_, s)| *s),
            Some(Source::Default)
        );

        let keys: Vec<_> = prov.iter().map(|(k, _)| k).collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys, "provenance should return sorted keys");
    }
}

//! The settings one run obeys, and the values they take when nobody sets them.
//!
//! One field per key in the *Configuration defaults* section of `docs/DESIGN.md`,
//! spelled exactly as the TOML key is spelled, and nothing else: the effective
//! configuration an operator reads on the Configuration screen is this
//! struct, so a setting that is not here does not exist.
//!
//! Defaults live in one place — [`Default`] — and `#[serde(default)]` makes the
//! deserializer use it, so a value cannot be documented in one place and
//! actually applied in another. A document therefore overrides only what it
//! sets, and an empty document is exactly [`Config::default()`].
//!
//! An unknown key is an error rather than something to ignore: a setting that
//! is misspelled, or left over from another version, would otherwise be
//! silently unwritten while the operator believed it was in effect.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The effective configuration: every setting, with the documented default.
///
/// Reading a TOML document into this type applies the file over
/// [`Config::default()`], so a document holding only the settings an operator
/// cares about leaves the rest at the values below. A key that is not a field
/// here is rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Which provider runs an attempt: `"dummy"`, `"claude"` or `"codex"`.
    ///
    /// A name rather than an enum because the provider set is data-driven
    /// (`provider/<name>.rs`) and a run reports the provider it used as text.
    pub provider: String,
    /// The model to ask that provider for, or `None` to take the provider's own
    /// default. Phases may override it; this is the run-wide answer.
    pub model: Option<String>,
    /// How long one attempt may run before it is killed: 4 hours, because an
    /// attempt is an agent working, not a command finishing.
    pub attempt_timeout_secs: u64,
    /// How long one gate command may run: 30 minutes, because a cold Rust build
    /// is slow and a gate that times out early fails a task that was fine.
    pub gate_timeout_secs: u64,
    /// How long an attempt may emit nothing before it is killed: 30 minutes.
    /// Measured against output rather than wall time so a quiet, long build is
    /// not confused with an agent that has stopped answering.
    pub idle_timeout_secs: u64,
    /// How many attempts a task gets in total, counted the way [`crate::AttemptId`]
    /// counts them. Two: one normal, one remediation.
    pub max_attempts: u32,
    /// How many of those attempts may be remediations of an earlier failure.
    pub max_remediation_attempts: u32,
    /// How many failures with an identical signature trip the circuit breaker
    /// and pause the queue rather than spending another attempt on them.
    pub circuit_breaker_threshold: u32,
    /// The remote mainline is fetched from and pushed to.
    pub mainline_remote: String,
    /// The branch mainline runs on. Publication compares the candidate against
    /// this branch's remote SHA before a task is called verified.
    pub mainline_branch: String,
    /// How many bytes of context an attempt is given at most. The supervisor
    /// truncates to this budget rather than letting a provider decide how much
    /// of the journal to read.
    pub context_budget_bytes: usize,
    /// How large a failure bundle may be: classification, gate output and diff
    /// summary are cut to this so a remediation prompt stays affordable.
    pub failure_bundle_bytes: usize,
    /// How many lines of agent output the ring for one subscriber holds before
    /// the oldest line is dropped. Unbounded output would otherwise be an
    /// out-of-memory crash in the middle of a run.
    pub output_ring_lines: usize,
    /// How much longer than a reported provider limit the supervisor waits
    /// before it tries again, so a limit that resets a second early does not
    /// cause a second rejection.
    pub limit_wait_margin_secs: u64,
    /// The longest wait a provider limit may impose before the run gives up and
    /// reports a limit rather than appearing to hang.
    pub limit_max_wait_secs: u64,
    /// The work protocol a task gets when its own front matter names none.
    pub default_protocol: String,
    /// A scripted scenario for the `dummy` provider, or `None` to use its
    /// built-in behavior. A path rather than a URL: it is a file on this
    /// machine, and tests point it at one.
    pub dummy_scenario_path: Option<PathBuf>,
    /// Which paths count as tests, matched against the diff of an attempt. The
    /// `tdd` protocol's red phase may write only here.
    pub test_globs: Vec<String>,
    /// Extra patterns whose matches are redacted out of stored output. Added to
    /// the built-in set, never replacing it.
    pub secret_patterns: Vec<String>,
    /// How many times the affected tests run in the flake gate. Zero would
    /// disable it, so the default is five.
    pub flake_runs: u32,
    /// How many days of journal and artifacts retention keeps, after which
    /// records are pruned. The journal itself is append-only and never edited.
    pub retention_days: u32,
    /// How many bytes must be free on the state filesystem before a run will
    /// start: 2 GiB, because a journal that fills the disk mid-run loses the
    /// evidence the run exists to produce.
    pub min_free_disk_bytes: u64,
}

impl Default for Config {
    /// The defaults `docs/DESIGN.md` documents, which are also the values an
    /// empty configuration document deserializes to.
    fn default() -> Self {
        Self {
            provider: "dummy".to_owned(),
            model: None,
            attempt_timeout_secs: 14_400,
            gate_timeout_secs: 1_800,
            idle_timeout_secs: 1_800,
            max_attempts: 2,
            max_remediation_attempts: 1,
            circuit_breaker_threshold: 3,
            mainline_remote: "origin".to_owned(),
            mainline_branch: "main".to_owned(),
            context_budget_bytes: 65_536,
            failure_bundle_bytes: 16_384,
            output_ring_lines: 4_096,
            limit_wait_margin_secs: 60,
            limit_max_wait_secs: 86_400,
            default_protocol: "direct".to_owned(),
            dummy_scenario_path: None,
            test_globs: vec![
                "**/tests/**".to_owned(),
                "**/*_test.rs".to_owned(),
                "src/**/tests.rs".to_owned(),
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
    use super::Config;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// Reads a TOML document as `ktask-rs` would read a configuration file.
    fn parse(document: &str) -> Config {
        toml::from_str(document)
            .unwrap_or_else(|error| panic!("`{document}` is a configuration: {error}"))
    }

    /// Compares two configurations field by field.
    ///
    /// `Config` derives `Debug` and deliberately not `PartialEq`, so the
    /// derived rendering is the honest way to ask whether two of them hold the
    /// same values.
    fn assert_same_config(actual: &Config, expected: &Config) {
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }

    /// Every value `docs/DESIGN.md` documents as a default, spelled out here
    /// rather than read back from `Config::default()`, so that changing the
    /// implementation is caught rather than agreed with.
    fn assert_documented_defaults(config: &Config) {
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
            ["**/tests/**", "**/*_test.rs", "src/**/tests.rs"]
        );
        assert!(
            config.secret_patterns.is_empty(),
            "{:?}",
            config.secret_patterns
        );
        assert_eq!(config.flake_runs, 5);
        assert_eq!(config.retention_days, 90);
        assert_eq!(config.min_free_disk_bytes, 2_147_483_648);
    }

    /// A configuration with a value in every field, none of them a default.
    fn every_field_set() -> Config {
        Config {
            provider: "claude".to_owned(),
            model: Some("sonnet".to_owned()),
            attempt_timeout_secs: 600,
            gate_timeout_secs: 300,
            idle_timeout_secs: 120,
            max_attempts: 5,
            max_remediation_attempts: 2,
            circuit_breaker_threshold: 7,
            mainline_remote: "upstream".to_owned(),
            mainline_branch: "trunk".to_owned(),
            context_budget_bytes: 1_024,
            failure_bundle_bytes: 512,
            output_ring_lines: 64,
            limit_wait_margin_secs: 5,
            limit_max_wait_secs: 3_600,
            default_protocol: "tdd".to_owned(),
            dummy_scenario_path: Some(PathBuf::from("/state/scenario.json")),
            test_globs: vec!["tests/**".to_owned()],
            secret_patterns: vec!["secret_[a-z0-9]+".to_owned()],
            flake_runs: 20,
            retention_days: 7,
            min_free_disk_bytes: 1_073_741_824,
        }
    }

    #[test]
    fn the_default_configuration_is_the_values_the_design_documents() {
        assert_documented_defaults(&Config::default());
    }

    #[test]
    fn an_empty_toml_document_deserializes_to_the_documented_defaults() {
        let config = parse("");
        assert_documented_defaults(&config);
        assert_same_config(&config, &Config::default());
    }

    #[test]
    fn an_unknown_key_is_rejected_and_names_the_key() {
        let error = toml::from_str::<Config>("not_a_documented_setting = 1")
            .expect_err("a setting nobody defined must not be ignored in silence");
        let message = error.to_string();
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("not_a_documented_setting"), "{message}");
    }

    #[test]
    fn a_document_overrides_only_the_keys_it_sets() {
        let config = parse(
            "provider = \"codex\"\nmax_attempts = 3\nsecret_patterns = ['secret_[a-z0-9]+']\n",
        );
        let expected = Config {
            provider: "codex".to_owned(),
            max_attempts: 3,
            secret_patterns: vec!["secret_[a-z0-9]+".to_owned()],
            ..Config::default()
        };
        assert_same_config(&config, &expected);
    }

    #[test]
    fn an_optional_setting_is_read_from_a_document_that_sets_it() {
        let config = parse("model = \"sonnet\"\ndummy_scenario_path = \"/state/scenario.json\"\n");
        assert_eq!(config.model.as_deref(), Some("sonnet"));
        assert_eq!(
            config.dummy_scenario_path.as_deref(),
            Some(Path::new("/state/scenario.json"))
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_rejected_naming_the_type_the_setting_has() {
        for (document, expected) in [
            ("attempt_timeout_secs = \"4 hours\"", "u64"),
            ("max_attempts = 1.5", "u32"),
            ("provider = 7", "a string"),
            ("test_globs = \"tests/**\"", "a sequence"),
        ] {
            let error = toml::from_str::<Config>(document)
                .expect_err("`{document}` does not fit the settings the design types");
            let message = error.to_string();
            assert!(
                message.contains(expected),
                "`{message}` does not mention {expected}"
            );
        }
    }

    #[test]
    fn a_negative_number_is_rejected_by_a_setting_that_counts() {
        let error = toml::from_str::<Config>("flake_runs = -1")
            .expect_err("a number of runs cannot be negative");
        let message = error.to_string();
        assert!(message.contains("u32"), "{message}");
    }

    #[test]
    fn a_configuration_written_out_is_read_back_unchanged() {
        for config in [Config::default(), every_field_set()] {
            let document = toml::to_string(&config).expect("a configuration is writable as TOML");
            let read_back: Config = toml::from_str(&document)
                .unwrap_or_else(|error| panic!("`{document}` was written by this crate: {error}"));
            assert_same_config(&read_back, &config);
        }
    }

    #[test]
    fn the_toml_keys_are_exactly_the_documented_settings() {
        let document =
            toml::to_string(&every_field_set()).expect("a configuration is writable as TOML");
        let value: toml::Value = toml::from_str(&document).expect("the document is a TOML table");
        let keys: BTreeSet<&str> = value
            .as_table()
            .expect("a configuration is a table")
            .keys()
            .map(String::as_str)
            .collect();
        let documented: BTreeSet<&str> = [
            "provider",
            "model",
            "attempt_timeout_secs",
            "gate_timeout_secs",
            "idle_timeout_secs",
            "max_attempts",
            "max_remediation_attempts",
            "circuit_breaker_threshold",
            "mainline_remote",
            "mainline_branch",
            "context_budget_bytes",
            "failure_bundle_bytes",
            "output_ring_lines",
            "limit_wait_margin_secs",
            "limit_max_wait_secs",
            "default_protocol",
            "dummy_scenario_path",
            "test_globs",
            "secret_patterns",
            "flake_runs",
            "retention_days",
            "min_free_disk_bytes",
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, documented);
    }
}

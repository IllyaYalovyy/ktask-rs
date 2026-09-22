//! The `Config` struct, its documented defaults, and layered loading.
//!
//! Every field mirrors a key in the Configuration defaults section of
//! `docs/DESIGN.md`. An empty TOML document deserializes to [`Config::default`];
//! an unknown key is a deserialization error, so a typo in a user's config
//! file is caught rather than silently ignored.
//!
//! [`Config::load`] merges four layers, each outranking the one before it:
//! compiled-in defaults, the global config file, the project config file and
//! `KTASK_*` environment variables (a fifth layer, command-line flags, is
//! named by [`Source::Flag`] but applied by a caller above ktask-core, since
//! flag parsing is not this crate's concern). [`Config::provenance`] reports
//! which layer won for every key, so the effective configuration is never a
//! mystery.

use crate::project::{Project, project_config_path};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

    /// Which layer resolved each key, populated by [`Config::load`].
    ///
    /// Empty on a `Config` built any other way (`Config::default`, or
    /// deserializing a single file directly), since there is then only one
    /// layer and nothing to report.
    #[serde(skip)]
    provenance: HashMap<String, Source>,
}

/// The layer that resolved a configuration value, in increasing precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The compiled-in default.
    Default,
    /// The global config file, `$XDG_CONFIG_HOME/ktask-rs/config.toml`.
    GlobalFile,
    /// The current project's own config file.
    ProjectFile,
    /// A `KTASK_<FIELD>` environment variable, uppercased from the field name.
    Env,
    /// A command-line flag. Never produced by [`Config::load`]; reserved for
    /// a caller above ktask-core that layers flag parsing on top.
    Flag,
}

/// A value paired with the layer that resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    /// The winning value.
    pub value: T,
    /// The layer that provided it.
    pub source: Source,
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
            provenance: HashMap::new(),
        }
    }
}

impl Config {
    /// Loads configuration by merging, in increasing precedence: compiled-in
    /// defaults, `global`, `project`, then `KTASK_<FIELD>` environment
    /// variables (the field name uppercased, e.g. `KTASK_MAX_ATTEMPTS`).
    ///
    /// `global` and `project` are each optional; when given, a missing file
    /// is treated as an absent layer, not an error, since a project need not
    /// have created one yet. An unknown key at any layer is rejected exactly
    /// as it would be in a lone file, because every layer is merged before
    /// deserializing into `Config`, which denies unknown fields.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when a present file cannot be read or
    /// parsed, when an environment variable cannot be parsed as its field's
    /// type, or when the merged result contains an unknown key.
    pub fn load(
        global: Option<&Path>,
        project: Option<&Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Config> {
        let mut merged = default_table();
        let mut sources: HashMap<String, Source> = merged
            .keys()
            .map(|key| (key.clone(), Source::Default))
            .collect();

        if let Some(path) = global {
            merge_file(path, &mut merged, &mut sources, Source::GlobalFile)?;
        }
        if let Some(path) = project {
            merge_file(path, &mut merged, &mut sources, Source::ProjectFile)?;
        }
        merge_env(env, &mut merged, &mut sources)?;

        let mut config: Config =
            toml::Value::Table(merged)
                .try_into()
                .map_err(|err: toml::de::Error| Error::Config {
                    key: "<merged configuration>".to_string(),
                    detail: err.to_string(),
                })?;
        config.provenance = sources;
        Ok(config)
    }

    /// Returns every configurable key with the layer that resolved it.
    ///
    /// On a `Config` not built by [`Config::load`] (e.g. `Config::default`,
    /// or deserializing a single file), every key reports [`Source::Default`],
    /// since there was only ever one layer.
    #[must_use]
    pub fn provenance(&self) -> Vec<(String, Source)> {
        all_field_names()
            .into_iter()
            .map(|key| {
                let source = self
                    .provenance
                    .get(&key)
                    .copied()
                    .unwrap_or(Source::Default);
                (key, source)
            })
            .collect()
    }
}

/// Loads `project`'s effective configuration: [`Config::load`] with the
/// global config file (`paths::config_file`), `project`'s own config file
/// ([`project_config_path`]) and the process environment.
///
/// # Errors
///
/// Returns [`Error::Config`] under the conditions of [`Config::load`], and
/// also when the global config path cannot be resolved (see
/// [`crate::paths::config_file`]).
pub fn load_for(project: &Project) -> Result<Config> {
    load_for_with(project, &|key| std::env::var(key).ok())
}

fn load_for_with(project: &Project, env: &dyn Fn(&str) -> Option<String>) -> Result<Config> {
    let global = crate::paths::config_file_with(env)?;
    let project_file = project_config_path(project);
    Config::load(Some(&global), Some(&project_file), env)
}

/// `Config::default()`, rendered as a TOML table.
///
/// `Option<T>` fields whose value is `None` are dropped by TOML
/// serialization, since TOML has no null; see [`all_field_names`] for how
/// those are still accounted for.
fn default_table() -> toml::Table {
    match toml::Value::try_from(Config::default()) {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

/// Every configurable key, independent of whether its default is present in
/// [`default_table`].
///
/// `model` and `dummy_scenario_path` are `Config`'s only `Option<T>` fields;
/// their `None` default has no TOML representation, so they are named here
/// explicitly. A new `Option<T>` field must be added to this list too.
fn all_field_names() -> Vec<String> {
    let mut names: Vec<String> = default_table().keys().cloned().collect();
    for optional in ["model", "dummy_scenario_path"] {
        if !names.iter().any(|name| name == optional) {
            names.push(optional.to_string());
        }
    }
    names
}

/// Merges the TOML table at `path` into `merged`, recording `source` for
/// every key it sets. A missing file is not an error.
fn merge_file(
    path: &Path,
    merged: &mut toml::Table,
    sources: &mut HashMap<String, Source>,
    source: Source,
) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let table: toml::Table = toml::from_str(&text).map_err(|err| Error::Config {
        key: path.display().to_string(),
        detail: err.to_string(),
    })?;
    for (key, value) in table {
        sources.insert(key.clone(), source);
        merged.insert(key, value);
    }
    Ok(())
}

/// Applies `KTASK_<FIELD>` overrides from `env` into `merged`.
fn merge_env(
    env: &dyn Fn(&str) -> Option<String>,
    merged: &mut toml::Table,
    sources: &mut HashMap<String, Source>,
) -> Result<()> {
    for key in all_field_names() {
        let var = format!("KTASK_{}", key.to_uppercase());
        let Some(raw) = env(&var) else { continue };
        let value = parse_env_value(merged.get(&key), &raw)
            .map_err(|detail| Error::Config { key: var, detail })?;
        merged.insert(key.clone(), value);
        sources.insert(key, Source::Env);
    }
    Ok(())
}

/// Parses a raw environment string into the TOML type of `current`, the
/// field's existing value. A field absent from `current` (only possible for
/// `Config`'s `Option<T>` fields, whose `None` default has no TOML
/// representation) is treated as a string, since both are string-based.
///
/// Only the TOML types `Config`'s fields actually use are handled: integers
/// (the `u32`/`u64` fields), arrays (the `Vec<String>` fields, taken as
/// comma-separated) and strings (everything else, including `Option<T>`).
/// Any other type is rejected rather than guessed at.
fn parse_env_value(
    current: Option<&toml::Value>,
    raw: &str,
) -> std::result::Result<toml::Value, String> {
    match current {
        Some(toml::Value::Integer(_)) => raw
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|err| err.to_string()),
        Some(toml::Value::Array(_)) => Ok(toml::Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| toml::Value::String(part.to_string()))
                .collect(),
        )),
        Some(toml::Value::String(_)) | None => Ok(toml::Value::String(raw.to_string())),
        Some(_) => Err("this field cannot be set from an environment variable".to_string()),
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

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn source_of(config: &Config, key: &str) -> Option<Source> {
        config
            .provenance()
            .into_iter()
            .find(|(name, _)| name == key)
            .map(|(_, source)| source)
    }

    #[test]
    fn load_with_no_layers_falls_back_to_defaults_with_default_source() {
        let config = Config::load(None, None, &no_env).expect("load");
        assert_eq!(config.provider, Config::default().provider);
        assert_eq!(source_of(&config, "provider"), Some(Source::Default));
    }

    #[test]
    fn each_layer_outranks_the_one_before_it_and_provenance_names_the_winner() {
        let global_dir = tempfile::tempdir().expect("tempdir");
        let global_path = global_dir.path().join("global.toml");
        let project_dir = tempfile::tempdir().expect("tempdir");
        let project_path = project_dir.path().join("project.toml");

        // Layer 1: defaults only.
        let config = Config::load(None, None, &no_env).expect("load");
        assert_eq!(config.provider, "dummy");
        assert_eq!(source_of(&config, "provider"), Some(Source::Default));

        // Layer 2: global file beats the default.
        std::fs::write(&global_path, r#"provider = "from-global""#).expect("write global");
        let config = Config::load(Some(&global_path), None, &no_env).expect("load");
        assert_eq!(config.provider, "from-global");
        assert_eq!(source_of(&config, "provider"), Some(Source::GlobalFile));

        // Layer 3: project file beats the global file.
        std::fs::write(&project_path, r#"provider = "from-project""#).expect("write project");
        let config = Config::load(Some(&global_path), Some(&project_path), &no_env).expect("load");
        assert_eq!(config.provider, "from-project");
        assert_eq!(source_of(&config, "provider"), Some(Source::ProjectFile));

        // Layer 4: an environment variable beats every file.
        let env = |key: &str| (key == "KTASK_PROVIDER").then(|| "from-env".to_string());
        let config = Config::load(Some(&global_path), Some(&project_path), &env).expect("load");
        assert_eq!(config.provider, "from-env");
        assert_eq!(source_of(&config, "provider"), Some(Source::Env));

        // Fields untouched by any layer are still Default.
        assert_eq!(source_of(&config, "max_attempts"), Some(Source::Default));
    }

    #[test]
    fn a_missing_global_or_project_file_is_not_an_error() {
        let missing = Path::new("/no/such/directory/config.toml");
        let config = Config::load(Some(missing), Some(missing), &no_env).expect("load");
        assert_eq!(config.provider, Config::default().provider);
        assert_eq!(source_of(&config, "provider"), Some(Source::Default));
    }

    #[test]
    fn an_unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Config::load(Some(dir.path()), None, &no_env).expect_err("must fail");
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn an_unknown_key_in_a_layered_file_is_still_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("global.toml");
        std::fs::write(&path, "bogus_key = 1").expect("write");
        let err = Config::load(Some(&path), None, &no_env).expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "<merged configuration>"));
        assert!(err.to_string().contains("bogus_key"));
    }

    #[test]
    fn malformed_toml_in_a_layered_file_names_the_file_and_the_syntax_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("global.toml");
        std::fs::write(&path, "provider = ").expect("write");
        let err = Config::load(Some(&path), None, &no_env).expect_err("must fail");
        let path_string = path.display().to_string();
        assert!(matches!(&err, Error::Config { key, .. } if *key == path_string));
    }

    #[test]
    fn env_overrides_an_integer_field() {
        let env = |key: &str| (key == "KTASK_MAX_ATTEMPTS").then(|| "9".to_string());
        let config = Config::load(None, None, &env).expect("load");
        assert_eq!(config.max_attempts, 9);
        assert_eq!(source_of(&config, "max_attempts"), Some(Source::Env));
    }

    #[test]
    fn env_overrides_an_array_field_as_a_comma_separated_list() {
        let env = |key: &str| (key == "KTASK_TEST_GLOBS").then(|| "a/**, b/**".to_string());
        let config = Config::load(None, None, &env).expect("load");
        assert_eq!(config.test_globs, vec!["a/**", "b/**"]);
        assert_eq!(source_of(&config, "test_globs"), Some(Source::Env));
    }

    #[test]
    fn env_overrides_an_option_field_that_has_no_default_representation() {
        let env = |key: &str| (key == "KTASK_MODEL").then(|| "opus".to_string());
        let config = Config::load(None, None, &env).expect("load");
        assert_eq!(config.model, Some("opus".to_string()));
        assert_eq!(source_of(&config, "model"), Some(Source::Env));
    }

    #[test]
    fn unset_option_field_still_reports_default_source() {
        let config = Config::load(None, None, &no_env).expect("load");
        assert_eq!(config.model, None);
        assert_eq!(source_of(&config, "model"), Some(Source::Default));
    }

    #[test]
    fn env_with_an_unparsable_integer_is_an_error() {
        let env = |key: &str| (key == "KTASK_MAX_ATTEMPTS").then(|| "not-a-number".to_string());
        let err = Config::load(None, None, &env).expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "KTASK_MAX_ATTEMPTS"));
    }

    #[test]
    fn provenance_on_a_default_config_reports_default_for_every_key() {
        let config = Config::default();
        assert!(
            config
                .provenance()
                .into_iter()
                .all(|(_, source)| source == Source::Default)
        );
    }

    fn project_with_state_dir(state_dir: PathBuf) -> Project {
        Project {
            root: state_dir.clone(),
            id: "test-project".to_string(),
            state_dir,
        }
    }

    fn env_with_xdg_config_home(dir: &tempfile::TempDir) -> impl Fn(&str) -> Option<String> {
        let config_home = dir.path().to_string_lossy().to_string();
        move |key| (key == "XDG_CONFIG_HOME").then(|| config_home.clone())
    }

    #[test]
    fn load_for_with_no_config_files_gets_documented_defaults() {
        let config_home = tempfile::tempdir().expect("config home");
        let state = tempfile::tempdir().expect("state dir");
        let project = project_with_state_dir(state.path().to_path_buf());
        let env = env_with_xdg_config_home(&config_home);

        let config = load_for_with(&project, &env).expect("load_for_with");

        assert_eq!(config.provider, Config::default().provider);
        assert_eq!(config.max_attempts, Config::default().max_attempts);
        assert!(
            config
                .provenance()
                .into_iter()
                .all(|(_, source)| source == Source::Default)
        );
    }

    #[test]
    fn load_for_with_project_file_overrides_global_file() {
        let config_home = tempfile::tempdir().expect("config home");
        let state = tempfile::tempdir().expect("state dir");
        let project = project_with_state_dir(state.path().to_path_buf());
        let env = env_with_xdg_config_home(&config_home);

        let global_path = crate::paths::config_file_with(&env).expect("global path");
        std::fs::create_dir_all(global_path.parent().expect("parent")).expect("mkdir global");
        std::fs::write(&global_path, r#"provider = "from-global""#).expect("write global");

        let config = load_for_with(&project, &env).expect("load_for_with");
        assert_eq!(config.provider, "from-global");
        assert_eq!(source_of(&config, "provider"), Some(Source::GlobalFile));

        std::fs::write(
            project_config_path(&project),
            r#"provider = "from-project""#,
        )
        .expect("write project");

        let config = load_for_with(&project, &env).expect("load_for_with");
        assert_eq!(config.provider, "from-project");
        assert_eq!(source_of(&config, "provider"), Some(Source::ProjectFile));
    }
}

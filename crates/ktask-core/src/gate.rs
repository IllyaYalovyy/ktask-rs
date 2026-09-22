//! `GateKind`, `Gate` and `Profile`: the runner's mechanical quality gates as
//! typed configuration, matching `docs/DESIGN.md`'s `gate.rs` pseudocode.
//!
//! A gate is a command the runner executes out-of-band from the agent,
//! per VISION.md §8. Every gate has its own timeout, working directory and
//! environment; a project's full set of gates is its [`Profile`]. Only
//! [`GateKind::Verify`] — "the mandatory, complete local suite. Not
//! optional, not skippable by config in strict mode" — is required to be
//! present; [`Profile::load`] rejects a profile that omits it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{Error, Result};

/// Which mechanical quality gate a [`Gate`] configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GateKind {
    /// Proves the project was green before the task started.
    Baseline,
    /// Fast edit-loop verification during the run.
    Targeted,
    /// The mandatory, complete local suite. Not optional, not skippable by
    /// config in strict mode.
    Verify,
    /// Static analysis / linting.
    Lint,
    /// Source formatting check.
    Format,
    /// Compiles or builds the project.
    Build,
    /// Scans staged files, tracked files and the outgoing commit range for
    /// forbidden paths and content patterns.
    Privacy,
}

/// One mechanical quality gate: the command the runner executes, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    /// Which gate this is.
    pub kind: GateKind,
    /// The command to run, as an argv. Never shell-interpreted.
    pub command: Vec<String>,
    /// Wall-clock limit for this gate, in seconds.
    pub timeout_secs: u64,
    /// The directory the command runs in. `None` runs it at the project
    /// root.
    pub working_dir: Option<PathBuf>,
    /// Environment variables set for the command, beyond whatever the
    /// runner's own process environment already provides.
    pub env: BTreeMap<String, String>,
}

/// A project's verification profile: every gate the runner may execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Profile {
    /// The gates this profile defines.
    pub gates: Vec<Gate>,
}

impl Profile {
    /// Returns the gate of the given kind, if this profile defines one.
    #[must_use]
    pub fn get(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|gate| gate.kind == kind)
    }

    /// Parses `text` as a TOML verification profile.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if `text` is not valid TOML for a
    /// `Profile`, or if the resulting profile has no [`GateKind::Verify`]
    /// gate: VISION.md §8 makes that gate mandatory, so its absence is a
    /// configuration error caught at load time rather than a profile that
    /// silently never verifies.
    pub fn load(text: &str) -> Result<Profile> {
        let profile: Profile = toml::from_str(text).map_err(|err| Error::Config {
            key: "profile".to_string(),
            detail: err.to_string(),
        })?;
        if profile.get(GateKind::Verify).is_none() {
            return Err(Error::Config {
                key: "gates".to_string(),
                detail: "a verification profile must define the mandatory Verify gate".to_string(),
            });
        }
        Ok(profile)
    }
}

/// Builds a [`Profile`] from `config`'s gate command fields (VISION.md §8):
/// each `Some` `*_command` field becomes a [`Gate`] of the corresponding
/// kind, sharing `config.gate_timeout_secs` as its timeout and running at
/// the project root with no extra environment. A `None` field is simply
/// absent from the profile.
///
/// `config.flake_command` has no corresponding `GateKind` yet — VISION.md
/// §14 places flaky-test wiring in v0.2/backlog scope — so it is not turned
/// into a gate here.
///
/// # Errors
///
/// Returns [`Error::Config`] if `config.verify_command` is `None`: VISION.md
/// §8 makes the Verify gate mandatory, so its absence is a configuration
/// error caught here rather than a profile that silently never verifies.
pub fn profile_from(config: &Config) -> Result<Profile> {
    let commands: [(GateKind, &Option<Vec<String>>); 7] = [
        (GateKind::Baseline, &config.baseline_command),
        (GateKind::Targeted, &config.targeted_test_command),
        (GateKind::Verify, &config.verify_command),
        (GateKind::Lint, &config.lint_command),
        (GateKind::Format, &config.format_command),
        (GateKind::Build, &config.build_command),
        (GateKind::Privacy, &config.privacy_command),
    ];

    let gates = commands
        .into_iter()
        .filter_map(|(kind, command)| {
            command.as_ref().map(|command| Gate {
                kind,
                command: command.clone(),
                timeout_secs: config.gate_timeout_secs,
                working_dir: None,
                env: BTreeMap::new(),
            })
        })
        .collect::<Vec<_>>();

    let profile = Profile { gates };
    if profile.get(GateKind::Verify).is_none() {
        return Err(Error::Config {
            key: "verify_command".to_string(),
            detail: "a verification profile must define the mandatory verify_command".to_string(),
        });
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(kind: GateKind) -> Gate {
        Gate {
            kind,
            command: vec!["true".to_string()],
            timeout_secs: 60,
            working_dir: None,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn get_returns_the_gate_of_the_requested_kind() {
        let profile = Profile {
            gates: vec![gate(GateKind::Verify), gate(GateKind::Lint)],
        };
        assert_eq!(
            profile.get(GateKind::Verify).unwrap().kind,
            GateKind::Verify
        );
        assert_eq!(profile.get(GateKind::Lint).unwrap().kind, GateKind::Lint);
    }

    #[test]
    fn get_returns_none_for_a_kind_the_profile_does_not_define() {
        let profile = Profile {
            gates: vec![gate(GateKind::Verify)],
        };
        assert!(profile.get(GateKind::Format).is_none());
    }

    #[test]
    fn a_profile_missing_the_verify_gate_is_a_configuration_error_at_load_time() {
        let profile = Profile {
            gates: vec![gate(GateKind::Lint), gate(GateKind::Build)],
        };
        let text = toml::to_string(&profile).expect("serialize");

        let err = Profile::load(&text).expect_err("must fail without a Verify gate");
        assert!(matches!(&err, Error::Config { key, .. } if key == "gates"));
        assert!(err.to_string().contains("Verify"));
    }

    #[test]
    fn an_empty_profile_is_a_configuration_error() {
        let text = toml::to_string(&Profile::default()).expect("serialize");
        let err = Profile::load(&text).expect_err("must fail without any gates");
        assert!(matches!(&err, Error::Config { key, .. } if key == "gates"));
    }

    #[test]
    fn a_profile_with_a_verify_gate_loads() {
        let profile = Profile {
            gates: vec![gate(GateKind::Verify)],
        };
        let text = toml::to_string(&profile).expect("serialize");
        let loaded = Profile::load(&text).expect("load");
        assert_eq!(loaded, profile);
    }

    #[test]
    fn malformed_toml_is_a_configuration_error() {
        let err = Profile::load("not valid toml =====").expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "profile"));
    }

    #[test]
    fn a_full_profile_round_trips_through_toml() {
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".to_string(), "warn".to_string());

        let profile = Profile {
            gates: vec![
                Gate {
                    kind: GateKind::Baseline,
                    command: vec!["cargo".to_string(), "check".to_string()],
                    timeout_secs: 1_800,
                    working_dir: Some(PathBuf::from("crates/ktask-core")),
                    env: env.clone(),
                },
                Gate {
                    kind: GateKind::Verify,
                    command: vec![
                        "cargo".to_string(),
                        "nextest".to_string(),
                        "run".to_string(),
                    ],
                    timeout_secs: 1_800,
                    working_dir: None,
                    env: BTreeMap::new(),
                },
                Gate {
                    kind: GateKind::Privacy,
                    command: vec!["ktask-rs".to_string(), "privacy-scan".to_string()],
                    timeout_secs: 120,
                    working_dir: None,
                    env,
                },
            ],
        };

        let text = toml::to_string(&profile).expect("serialize");
        let round_tripped = Profile::load(&text).expect("load");
        assert_eq!(round_tripped, profile);
    }

    fn cmd(word: &str) -> Vec<String> {
        vec![word.to_string()]
    }

    #[test]
    fn profile_from_builds_a_gate_for_every_configured_command() {
        let mut config = Config::default();
        config.baseline_command = Some(cmd("baseline"));
        config.targeted_test_command = Some(cmd("targeted"));
        config.verify_command = Some(cmd("verify"));
        config.lint_command = Some(cmd("lint"));
        config.format_command = Some(cmd("format"));
        config.build_command = Some(cmd("build"));
        config.privacy_command = Some(cmd("privacy"));

        let profile = profile_from(&config).expect("profile");

        let kinds: Vec<GateKind> = profile.gates.iter().map(|gate| gate.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GateKind::Baseline,
                GateKind::Targeted,
                GateKind::Verify,
                GateKind::Lint,
                GateKind::Format,
                GateKind::Build,
                GateKind::Privacy,
            ]
        );
        assert_eq!(profile.get(GateKind::Lint).unwrap().command, cmd("lint"));
    }

    #[test]
    fn profile_from_omits_gates_for_unconfigured_commands() {
        let mut config = Config::default();
        config.verify_command = Some(cmd("verify"));

        let profile = profile_from(&config).expect("profile");

        assert_eq!(profile.gates.len(), 1);
        assert_eq!(profile.gates[0].kind, GateKind::Verify);
    }

    #[test]
    fn profile_from_without_verify_command_is_a_configuration_error() {
        let mut config = Config::default();
        config.lint_command = Some(cmd("lint"));

        let err = profile_from(&config).expect_err("must fail without verify_command");
        assert!(matches!(&err, Error::Config { key, .. } if key == "verify_command"));
        assert!(err.to_string().contains("verify_command"));
    }

    #[test]
    fn profile_from_an_empty_config_is_a_configuration_error() {
        let err = profile_from(&Config::default()).expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "verify_command"));
    }

    #[test]
    fn profile_from_uses_gate_timeout_secs_from_config() {
        let mut config = Config::default();
        config.verify_command = Some(cmd("verify"));
        config.gate_timeout_secs = 42;

        let profile = profile_from(&config).expect("profile");

        assert_eq!(profile.get(GateKind::Verify).unwrap().timeout_secs, 42);
    }

    #[test]
    fn profile_from_ignores_flake_command_since_no_gate_kind_covers_it() {
        let mut config = Config::default();
        config.verify_command = Some(cmd("verify"));
        config.flake_command = Some(cmd("flake"));

        let profile = profile_from(&config).expect("profile");

        assert_eq!(profile.gates.len(), 1);
        assert_eq!(profile.gates[0].kind, GateKind::Verify);
    }
}

//! Gate definitions and verification profiles.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Mechanical quality gate kinds, as defined in VISION.md section 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GateKind {
    /// Baseline: prove the project was green before the task started.
    Baseline,
    /// Targeted: fast edit-loop verification during the run.
    Targeted,
    /// Verify: mandatory, complete local suite. Required for all profiles.
    Verify,
    /// Lint: lint command execution.
    Lint,
    /// Format: format command execution.
    Format,
    /// Build: build command execution.
    Build,
    /// Privacy: scan for forbidden paths and content patterns.
    Privacy,
    /// Flake: repeated or randomized execution of affected tests.
    Flake,
}

/// A gate command with its configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    /// The gate kind.
    pub kind: GateKind,
    /// Command to execute, as a list of arguments.
    pub command: Vec<String>,
    /// Timeout in seconds.
    pub timeout_secs: u64,
    /// Working directory for execution, if specified.
    pub working_dir: Option<PathBuf>,
    /// Environment variables to set for the gate execution.
    pub env: BTreeMap<String, String>,
}

/// A verification profile containing gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Gates in this profile.
    pub gates: Vec<Gate>,
}

impl Profile {
    /// Get a gate by its kind.
    #[must_use]
    pub fn get(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|g| g.kind == kind)
    }

    /// Validate that the profile has the mandatory Verify gate.
    ///
    /// # Errors
    ///
    /// Returns an error if the Verify gate is missing from the profile.
    pub fn validate(&self) -> crate::Result<()> {
        if self.get(GateKind::Verify).is_none() {
            return Err(crate::Error::Config {
                key: "gates".to_string(),
                detail: "Verify gate is mandatory and must be present in the profile".to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_get_returns_gate_by_kind() {
        let gate = Gate {
            kind: GateKind::Verify,
            command: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 300,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let profile = Profile {
            gates: vec![gate.clone()],
        };
        assert_eq!(profile.get(GateKind::Verify), Some(&gate));
        assert_eq!(profile.get(GateKind::Baseline), None);
    }

    #[test]
    fn profile_validation_requires_verify_gate() {
        let baseline_gate = Gate {
            kind: GateKind::Baseline,
            command: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 300,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let profile = Profile {
            gates: vec![baseline_gate],
        };
        let result = profile.validate();
        assert!(result.is_err());
        if let Err(crate::Error::Config { key, detail }) = result {
            assert_eq!(key, "gates");
            assert!(detail.contains("Verify"));
        }
    }

    #[test]
    fn profile_validation_passes_with_verify_gate() {
        let verify_gate = Gate {
            kind: GateKind::Verify,
            command: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 300,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let profile = Profile {
            gates: vec![verify_gate],
        };
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn gate_kind_roundtrips_through_serde() {
        let kinds = [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Verify,
            GateKind::Lint,
            GateKind::Format,
            GateKind::Build,
            GateKind::Privacy,
            GateKind::Flake,
        ];

        for kind in &kinds {
            let json = serde_json::to_string(kind).expect("serialize");
            let deserialized: GateKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*kind, deserialized);
        }
    }

    #[test]
    fn profile_roundtrips_through_toml() {
        use toml;

        let profile_toml = r#"
[[gates]]
kind = "Verify"
command = ["cargo", "test"]
timeout_secs = 300
env = {}

[[gates]]
kind = "Lint"
command = ["cargo", "clippy"]
timeout_secs = 60
env = {}
"#;

        let profile: Profile = toml::from_str(profile_toml).expect("deserialize");
        assert_eq!(profile.gates.len(), 2);
        assert!(profile.get(GateKind::Verify).is_some());
        assert!(profile.get(GateKind::Lint).is_some());

        let serialized = toml::to_string(&profile).expect("serialize");
        let deserialized: Profile = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized.gates.len(), 2);
    }
}

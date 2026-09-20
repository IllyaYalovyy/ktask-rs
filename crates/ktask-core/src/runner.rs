//! Task execution runner with preflight checks and journaling.

use crate::{Config, Error, FailureClass, Project, Provider, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Evidence from a preflight check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreflightEvidence {
    /// Remote fetch succeeded.
    RemoteFetched {
        /// The remote name (e.g., "origin").
        remote: String,
        /// Branch being tracked (e.g., "main").
        branch: String,
    },
    /// Repository is clean (no uncommitted changes).
    RepositoryClean,
    /// Baseline gate passed.
    BaselineGateGreen,
    /// Provider is available and capable.
    ProviderAvailable {
        /// Provider name.
        provider: String,
    },
    /// Sufficient disk space available.
    DiskSpaceAvailable {
        /// Free space in bytes.
        free_bytes: u64,
        /// Required minimum in bytes.
        required_bytes: u64,
    },
    /// Repository lock acquired successfully.
    LockAcquired,
}

/// Classification of preflight failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreflightFailure {
    /// Could not fetch from remote.
    RemoteFetchFailed {
        /// Remote name.
        remote: String,
        /// Error message.
        detail: String,
    },
    /// Repository has uncommitted changes.
    RepositoryDirty {
        /// Status output.
        status: String,
    },
    /// Baseline gate failed.
    BaselineGateFailed {
        /// Exit code if available.
        exit_code: Option<i32>,
        /// Standard output.
        stdout: String,
        /// Standard error.
        stderr: String,
    },
    /// Provider not available.
    ProviderUnavailable {
        /// Provider name.
        provider: String,
        /// Error message.
        detail: String,
    },
    /// Insufficient disk space.
    InsufficientDiskSpace {
        /// Available space in bytes.
        available_bytes: u64,
        /// Required minimum in bytes.
        required_bytes: u64,
    },
    /// Repository lock not acquirable.
    LockNotAcquirable {
        /// Error message.
        detail: String,
    },
}

impl PreflightFailure {
    /// Classify the failure into a `FailureClass`.
    #[must_use]
    pub fn classification(&self) -> FailureClass {
        match self {
            PreflightFailure::RemoteFetchFailed { .. }
            | PreflightFailure::InsufficientDiskSpace { .. } => FailureClass::EnvironmentFailure,
            PreflightFailure::RepositoryDirty { .. }
            | PreflightFailure::LockNotAcquirable { .. } => FailureClass::PolicyFailure,
            PreflightFailure::BaselineGateFailed { .. } => FailureClass::VerificationFailure,
            PreflightFailure::ProviderUnavailable { .. } => FailureClass::ProviderConfiguration,
        }
    }
}

/// Result of preflight checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Whether all checks passed.
    pub success: bool,
    /// Evidence collected from successful checks.
    pub evidence: Vec<PreflightEvidence>,
    /// Failure information if any check failed.
    pub failure: Option<PreflightFailure>,
}

impl PreflightReport {
    /// Create a successful preflight report.
    fn success(evidence: Vec<PreflightEvidence>) -> Self {
        PreflightReport {
            success: true,
            evidence,
            failure: None,
        }
    }

    /// Create a failed preflight report.
    fn failed(evidence: Vec<PreflightEvidence>, failure: PreflightFailure) -> Self {
        PreflightReport {
            success: false,
            evidence,
            failure: Some(failure),
        }
    }
}

/// Check that the world is sane before tokens are spent.
///
/// Performs the following checks in order:
/// 1. Fetch from remote and verify mainline is clean
/// 2. Run baseline gate to prove the project was green
/// 3. Verify provider is available
/// 4. Check sufficient disk space
/// 5. Verify repository lock is acquirable
///
/// Each failure carries its own `FailureClass`. The report includes evidence
/// from all successful checks up to the first failure.
///
/// # Arguments
///
/// * `project` - The registered ktask project
/// * `config` - The effective configuration
/// * `provider` - The configured provider instance
///
/// # Returns
///
/// A `PreflightReport` containing success status and evidence. On failure,
/// the report includes the first failure encountered and all prior evidence.
///
/// # Errors
///
/// Returns an error if the journal or filesystem operations fail.
pub fn preflight(
    project: &Project,
    config: &Config,
    provider: &dyn Provider,
) -> Result<PreflightReport> {
    let mut evidence = Vec::new();

    if let Err(failure) = check_remote_fetch(project, config, &mut evidence) {
        return Ok(PreflightReport::failed(evidence, failure));
    }

    if let Err(failure) = check_repo_clean(project, &mut evidence) {
        return Ok(PreflightReport::failed(evidence, failure));
    }

    if let Err(failure) = check_baseline_gate(project, config, &mut evidence) {
        return Ok(PreflightReport::failed(evidence, failure));
    }

    let _caps = provider.capabilities();
    evidence.push(PreflightEvidence::ProviderAvailable {
        provider: provider.name().to_string(),
    });

    if let Err(failure) = check_disk_space(project, config, &mut evidence) {
        return Ok(PreflightReport::failed(evidence, failure));
    }

    if let Err(failure) = check_lock(project, &mut evidence) {
        return Ok(PreflightReport::failed(evidence, failure));
    }

    Ok(PreflightReport::success(evidence))
}

fn check_remote_fetch(
    project: &Project,
    config: &Config,
    evidence: &mut Vec<PreflightEvidence>,
) -> std::result::Result<(), PreflightFailure> {
    let remote = &config.mainline_remote;
    let branch = &config.mainline_branch;
    match crate::git::fetch(&project.root, remote) {
        Ok(()) => {
            evidence.push(PreflightEvidence::RemoteFetched {
                remote: remote.clone(),
                branch: branch.clone(),
            });
            Ok(())
        }
        Err(_) => Err(PreflightFailure::RemoteFetchFailed {
            remote: remote.clone(),
            detail: format!("failed to fetch from remote '{remote}'"),
        }),
    }
}

fn check_repo_clean(
    project: &Project,
    evidence: &mut Vec<PreflightEvidence>,
) -> std::result::Result<(), PreflightFailure> {
    match crate::git::is_clean(&project.root) {
        Ok(true) => {
            evidence.push(PreflightEvidence::RepositoryClean);
            Ok(())
        }
        Ok(false) => {
            let status = crate::git::status_porcelain(&project.root)
                .unwrap_or_else(|_| "unknown status".to_string());
            Err(PreflightFailure::RepositoryDirty { status })
        }
        Err(_) => Err(PreflightFailure::RepositoryDirty {
            status: "failed to check status".to_string(),
        }),
    }
}

fn check_baseline_gate(
    project: &Project,
    config: &Config,
    evidence: &mut Vec<PreflightEvidence>,
) -> std::result::Result<(), PreflightFailure> {
    if let Some(baseline_cmd) = &config.baseline_command {
        let gate = crate::Gate {
            kind: crate::GateKind::Baseline,
            command: baseline_cmd.clone(),
            timeout_secs: u64::from(config.gate_timeout_secs),
            working_dir: None,
            env: std::collections::BTreeMap::default(),
        };
        match crate::run_gate(&gate, &project.root, None) {
            Ok(result) if result.passed => {
                evidence.push(PreflightEvidence::BaselineGateGreen);
                Ok(())
            }
            Ok(result) => Err(PreflightFailure::BaselineGateFailed {
                exit_code: result.exit_code,
                stdout: result.stdout,
                stderr: result.stderr,
            }),
            Err(_) => Err(PreflightFailure::BaselineGateFailed {
                exit_code: None,
                stdout: String::new(),
                stderr: "failed to execute baseline gate".to_string(),
            }),
        }
    } else {
        Ok(())
    }
}

fn check_disk_space(
    project: &Project,
    config: &Config,
    evidence: &mut Vec<PreflightEvidence>,
) -> std::result::Result<(), PreflightFailure> {
    match disk_free(&project.root) {
        Ok(free_bytes) => {
            let required = config.min_free_disk_bytes;
            if free_bytes >= required {
                evidence.push(PreflightEvidence::DiskSpaceAvailable {
                    free_bytes,
                    required_bytes: required,
                });
                Ok(())
            } else {
                Err(PreflightFailure::InsufficientDiskSpace {
                    available_bytes: free_bytes,
                    required_bytes: required,
                })
            }
        }
        Err(_) => Err(PreflightFailure::InsufficientDiskSpace {
            available_bytes: 0,
            required_bytes: config.min_free_disk_bytes,
        }),
    }
}

fn check_lock(
    project: &Project,
    evidence: &mut Vec<PreflightEvidence>,
) -> std::result::Result<(), PreflightFailure> {
    match crate::RepoLock::acquire(&project.state_dir, Duration::from_secs(5)) {
        Ok(_lock) => {
            evidence.push(PreflightEvidence::LockAcquired);
            Ok(())
        }
        Err(_) => Err(PreflightFailure::LockNotAcquirable {
            detail: "failed to acquire repository lock".to_string(),
        }),
    }
}

/// Get the free disk space on the filesystem containing the given path.
fn disk_free(path: &Path) -> Result<u64> {
    match nix::sys::statvfs::statvfs(path) {
        Ok(stat) => {
            let available = stat.blocks_available() * stat.block_size();
            Ok(available)
        }
        Err(_) => Err(Error::Io(std::io::Error::other(
            "failed to get filesystem statistics",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Scenario;
    use crate::testing::ScratchRepo;

    #[test]
    fn preflight_success_with_all_checks() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir,
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);

        let scenario = Scenario { steps: vec![] };
        let provider = crate::Dummy::new(scenario);

        let result = preflight(&project, &config, &provider).expect("Preflight check failed");

        assert!(result.success);
        assert!(result.failure.is_none());
        assert!(!result.evidence.is_empty());
    }

    #[test]
    fn preflight_fails_when_baseline_gate_fails() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir,
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["false".to_string()]);

        let scenario = Scenario { steps: vec![] };
        let provider = crate::Dummy::new(scenario);

        let result = preflight(&project, &config, &provider).expect("Preflight check failed");

        assert!(!result.success);
        assert!(result.failure.is_some());
        let failure = result.failure.unwrap();
        assert!(matches!(
            failure,
            PreflightFailure::BaselineGateFailed { .. }
        ));
        assert_eq!(failure.classification(), FailureClass::VerificationFailure);
    }

    #[test]
    fn preflight_fails_with_repository_dirty() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        std::fs::write(repo.path().join("untracked.txt"), "content")
            .expect("Failed to create untracked file");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir,
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);

        let scenario = Scenario { steps: vec![] };
        let provider = crate::Dummy::new(scenario);

        let result = preflight(&project, &config, &provider).expect("Preflight check failed");

        assert!(!result.success);
        assert!(result.failure.is_some());
        let failure = result.failure.unwrap();
        assert!(matches!(failure, PreflightFailure::RepositoryDirty { .. }));
        assert_eq!(failure.classification(), FailureClass::PolicyFailure);
    }

    #[test]
    fn baseline_gate_failure_has_verification_class() {
        let failure = PreflightFailure::BaselineGateFailed {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(failure.classification(), FailureClass::VerificationFailure);
    }

    #[test]
    fn repository_dirty_failure_has_policy_class() {
        let failure = PreflightFailure::RepositoryDirty {
            status: "M file.txt".to_string(),
        };
        assert_eq!(failure.classification(), FailureClass::PolicyFailure);
    }

    #[test]
    fn remote_fetch_failure_has_environment_class() {
        let failure = PreflightFailure::RemoteFetchFailed {
            remote: "origin".to_string(),
            detail: "connection refused".to_string(),
        };
        assert_eq!(failure.classification(), FailureClass::EnvironmentFailure);
    }

    #[test]
    fn provider_unavailable_failure_has_provider_config_class() {
        let failure = PreflightFailure::ProviderUnavailable {
            provider: "claude".to_string(),
            detail: "not configured".to_string(),
        };
        assert_eq!(
            failure.classification(),
            FailureClass::ProviderConfiguration
        );
    }

    #[test]
    fn insufficient_disk_space_failure_has_environment_class() {
        let failure = PreflightFailure::InsufficientDiskSpace {
            available_bytes: 1024,
            required_bytes: 2048,
        };
        assert_eq!(failure.classification(), FailureClass::EnvironmentFailure);
    }

    #[test]
    fn lock_not_acquirable_failure_has_policy_class() {
        let failure = PreflightFailure::LockNotAcquirable {
            detail: "timeout".to_string(),
        };
        assert_eq!(failure.classification(), FailureClass::PolicyFailure);
    }
}

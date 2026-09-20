//! Task execution runner with preflight checks and journaling.

use crate::{
    AttemptId, AttemptRecord, Bus, Config, Error, FailureClass, PhaseSpec, Project, Protocol,
    Provider, Recorder, RepoLock, Result, Task,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;

/// The outcome of running a single phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhaseOutcome {
    /// Phase completed successfully.
    Success,
    /// Phase failed with a classified failure.
    Failure {
        /// Classification of the failure.
        class: FailureClass,
        /// Detailed description of the failure.
        detail: String,
    },
}

/// The runner for executing tasks with journaling and verification.
///
/// The runner coordinates all components needed to execute a task:
/// - Project configuration and state directory
/// - Configuration loading and validation
/// - Quality gates (baseline, verify, lint, format, build, privacy)
/// - Event journaling and broadcasting
/// - Provider interaction for AI agents
pub struct Runner {
    /// The registered project.
    pub project: Project,
    /// Effective configuration (global + project overrides).
    pub config: Config,
    /// Quality gates profile (verification commands, timeouts, etc).
    pub profile: crate::Profile,
    /// Event recorder (journal + bus).
    pub recorder: Recorder,
    /// The configured provider for AI agents.
    pub provider: Box<dyn Provider>,
}

/// Result of preflight checks and worktree preparation.
#[derive(Debug)]
pub struct Prepared {
    /// Path to the created worktree.
    pub worktree_path: PathBuf,
    /// Base commit SHA for the worktree.
    pub base_sha: String,
    /// Repository lock held for this preparation.
    pub lock: RepoLock,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("project", &self.project)
            .field("config", &self.config)
            .field("profile", &self.profile)
            .field("recorder", &self.recorder)
            .field("provider", &self.provider.name())
            .finish()
    }
}

impl Runner {
    /// Create a new runner for the given project.
    ///
    /// Initializes all runner components:
    /// - Opens the event journal
    /// - Creates the event bus and subscribes
    /// - Loads the effective configuration
    /// - Builds the quality gates profile
    /// - Initializes the configured provider
    ///
    /// # Arguments
    ///
    /// * `project` - A registered ktask project
    ///
    /// # Errors
    ///
    /// Returns an error if the journal cannot be opened, configuration
    /// cannot be loaded, the gates profile is invalid, or the provider
    /// cannot be built.
    pub fn new(project: Project) -> Result<Runner> {
        let journal = crate::Journal::open_for(&project)?;
        let bus = Bus::new(1000);
        let _subscription = bus.subscribe();
        let config = crate::config::load_for(&project)?;
        let profile = crate::gate::profile_from(&config)?;
        let provider = crate::provider::build(&config)?;

        let recorder = Recorder::new(journal, bus);

        Ok(Runner {
            project,
            config,
            profile,
            recorder,
            provider,
        })
    }

    /// Begin an attempt at a task, recording the start event.
    ///
    /// Records an `AttemptStarted` event with:
    /// - The protocol selected for this task
    /// - The current process ID
    /// - The base commit SHA before any changes
    ///
    /// The attempt evidence directory is created with the initial record.
    /// Each call returns a new attempt ID (1-based within the task).
    ///
    /// # Arguments
    ///
    /// * `task` - The task being attempted
    ///
    /// # Errors
    ///
    /// Returns an error if the protocol cannot be resolved, the SHA
    /// cannot be determined, the event cannot be journaled, or the
    /// evidence directory structure cannot be created.
    pub fn begin_attempt(&mut self, task: &Task) -> Result<AttemptId> {
        let protocol = Protocol::for_task(task, &self.config)?;
        let pid = std::process::id();
        let base_sha = crate::git::head_sha(&self.project.root)?;

        let attempt_id = AttemptId::new(1);

        self.recorder.record(
            Some(task.id),
            crate::EventKind::AttemptStarted {
                attempt: attempt_id,
                protocol: protocol.name,
                pid,
                base_sha: base_sha.clone(),
            },
        )?;

        let record = AttemptRecord {
            id: attempt_id,
            task: task.id,
            started: OffsetDateTime::now_utc(),
            ended: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            exit_reason: "attempt_started".to_string(),
            gates: vec![],
            usage: None,
            base_sha,
            candidate_sha: None,
        };

        let context = String::new();
        crate::attempt::write_evidence(&self.project, &record, &context)?;

        Ok(attempt_id)
    }

    /// Prepare for task execution by running preflight checks and creating a worktree.
    ///
    /// Performs preflight validation to ensure the world is sane before spending tokens:
    /// - Records preflight started event
    /// - Runs all preflight checks
    /// - On success, records preflight passed event
    /// - Acquires the repository lock
    /// - Creates a worktree checked out at the fetched remote SHA
    ///
    /// If preflight fails, the method records the failure classification and releases the lock.
    ///
    /// # Arguments
    ///
    /// * `task` - The task being prepared
    ///
    /// # Errors
    ///
    /// Returns an error if preflight checks fail, the lock cannot be acquired,
    /// or the worktree cannot be created. On preflight failure, returns a failure
    /// classification in the error.
    pub fn prepare(&mut self, task: &Task) -> Result<Prepared> {
        self.recorder
            .record(Some(task.id), crate::EventKind::PreflightStarted)?;

        let report = preflight(&self.project, &self.config, self.provider.as_ref())?;

        if !report.success {
            match report.failure {
                Some(failure) => {
                    let class = failure.classification();
                    let detail = format!("{failure:?}");

                    self.recorder.record(
                        Some(task.id),
                        crate::EventKind::PreflightFailed { class, detail },
                    )?;

                    return Err(Error::Policy {
                        detail: format!("Preflight check failed: {failure:?}"),
                        paths: vec![],
                    });
                }
                None => {
                    return Err(Error::Policy {
                        detail:
                            "Preflight failed with success=false but no failure details provided"
                                .to_string(),
                        paths: vec![],
                    });
                }
            }
        }

        let base_sha = crate::git::head_sha(&self.project.root)?;

        self.recorder.record(
            Some(task.id),
            crate::EventKind::PreflightPassed {
                base_sha: base_sha.clone(),
            },
        )?;

        let lock = RepoLock::acquire(&self.project.state_dir, Duration::from_secs(5))?;

        let remote_ref = format!(
            "refs/remotes/{}/{}",
            self.config.mainline_remote, self.config.mainline_branch
        );
        let remote_sha = crate::git::git(&self.project.root, &["rev-parse", &remote_ref])?;

        let worktree_name = format!("task-{}", task.id);
        let worktree_path =
            crate::git::create_worktree(&self.project.root, &worktree_name, &remote_sha)?;

        Ok(Prepared {
            worktree_path,
            base_sha,
            lock,
        })
    }

    /// Run a single phase of task execution.
    ///
    /// Executes a phase by:
    /// 1. Recording the phase entry
    /// 2. Assembling the context and prompt
    /// 3. Creating the report directory
    /// 4. Invoking the provider
    /// 5. Validating the model IDs
    /// 6. Reading and parsing the report
    /// 7. Checking that changed paths respect the write scope
    ///
    /// # Arguments
    ///
    /// * `prep` - The prepared execution state (worktree, lock, base SHA)
    /// * `task` - The task being executed
    /// * `attempt` - The current attempt number
    /// * `spec` - The phase specification with write scope and gates
    ///
    /// # Errors
    ///
    /// Returns a `PhaseOutcome::Failure` if:
    /// - The report is missing or cannot be parsed
    /// - Changed paths violate the write scope
    /// - The model ID check fails
    pub fn run_phase(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        spec: &PhaseSpec,
    ) -> Result<PhaseOutcome> {
        // Record phase entry
        self.recorder.record(
            Some(task.id),
            crate::EventKind::PhaseEntered {
                attempt,
                phase: spec.phase,
            },
        )?;

        // Collect ADRs from the repository
        let adrs = crate::collect_adrs(&self.project.root)?;

        // Load the context document from the prompt library
        let context_doc = Self::load_context_doc()?;

        // Load the template
        let template = crate::load_template(&self.project)?;

        // Assemble the prompt
        let prompt = crate::assemble(task, &context_doc, &adrs, &template, attempt, 1);

        // Create the report directory
        let report_path = crate::report_path(&self.project, task.id, attempt);
        if let Some(parent) = report_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Invoke the provider
        let invocation = crate::provider::Invocation {
            prompt,
            model: self.config.model.clone(),
            working_dir: prep.worktree_path.clone(),
        };

        let outcome = self.provider.invoke(&invocation, None)?;

        // Record agent output
        if !outcome.stdout.is_empty() {
            self.recorder.record(
                Some(task.id),
                crate::EventKind::AgentOutput {
                    attempt,
                    stream: crate::Stream::Stdout,
                    text: outcome.stdout.clone(),
                },
            )?;
        }

        if !outcome.stderr.is_empty() {
            self.recorder.record(
                Some(task.id),
                crate::EventKind::AgentOutput {
                    attempt,
                    stream: crate::Stream::Stderr,
                    text: outcome.stderr.clone(),
                },
            )?;
        }

        // Check model ID consistency
        let configured_model = self.config.model.as_deref();
        let reported_model = outcome.session_id.as_deref();
        crate::provider::check_model(configured_model, reported_model)?;

        // Read the report
        let report_content = match std::fs::read_to_string(&report_path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PhaseOutcome::Failure {
                    class: FailureClass::VerificationFailure,
                    detail: "Agent did not produce a report at the expected location".to_string(),
                });
            }
            Err(e) => return Err(Error::Io(e)),
        };

        // Parse the report
        let report_result = match crate::report::parse_report(&report_content) {
            Ok(r) => r,
            Err(e) => {
                return Ok(PhaseOutcome::Failure {
                    class: FailureClass::VerificationFailure,
                    detail: format!("Failed to parse report: {e}"),
                });
            }
        };

        // If the report indicates failure or needs input, return that as a failure
        match report_result {
            crate::ReportResult::Failed => {
                return Ok(PhaseOutcome::Failure {
                    class: FailureClass::AgentFailure,
                    detail: "Agent reported failure in the task".to_string(),
                });
            }
            crate::ReportResult::NeedsInput => {
                return Ok(PhaseOutcome::Failure {
                    class: FailureClass::NeedsInput,
                    detail: "Agent reported that input is needed".to_string(),
                });
            }
            crate::ReportResult::Done => {
                // Continue to scope check
            }
        }

        // Check that changed paths respect the write scope
        let changed_paths = crate::git::changed_paths(&prep.worktree_path, &prep.base_sha)?;
        match crate::protocol::check_scope(
            spec.write_scope,
            &changed_paths,
            &self.config.test_globs,
        ) {
            Ok(()) => Ok(PhaseOutcome::Success),
            Err(Error::Policy { detail, paths }) => Ok(PhaseOutcome::Failure {
                class: FailureClass::PolicyFailure,
                detail: format!(
                    "{} (offending files: {})",
                    detail,
                    paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
            Err(e) => Err(e),
        }
    }

    /// Execute a phase's gate and verify its results.
    ///
    /// Runs the gate command specified in the phase spec and verifies the results based
    /// on the phase type (red or green). Records gate execution events and returns the
    /// test summary.
    ///
    /// For red phases: verifies that new failing tests were introduced.
    /// For green phases: verifies that expected tests pass and no regression occurred.
    ///
    /// # Arguments
    ///
    /// * `prep` - The prepared execution state (worktree, lock, base SHA)
    /// * `task_id` - The task ID for recording events
    /// * `attempt` - The current attempt ID
    /// * `spec` - The phase specification with gate information
    /// * `before` - Previous test summary for comparison (required for red/green phases)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The gate command cannot be found or executed
    /// - The test output cannot be parsed
    /// - Red phase has no new failing tests
    /// - Green phase fails to pass expected tests or regresses others
    pub fn gate_phase(
        &mut self,
        prep: &Prepared,
        task_id: crate::TaskId,
        attempt: AttemptId,
        spec: &PhaseSpec,
        before: Option<&crate::gate::TestSummary>,
    ) -> Result<crate::gate::TestSummary> {
        // Get the gate for this phase
        let gate = spec
            .gate
            .ok_or_else(|| Error::Gate {
                kind: format!("{:?}", spec.phase),
                detail: "Phase has no gate configured".to_string(),
            })?;

        let gate_spec = self.profile.get(gate).ok_or_else(|| Error::Gate {
            kind: format!("{:?}", gate),
            detail: "Gate not found in profile".to_string(),
        })?;

        // Compute tree hash before gate (using git rev-parse)
        let tree_hash_before =
            crate::git::git(&prep.worktree_path, &["rev-parse", "HEAD^{tree}"])?;

        // Record gate started
        self.recorder.record(
            Some(task_id),
            crate::EventKind::GateStarted {
                attempt,
                gate_kind: gate,
                tree_hash: tree_hash_before,
            },
        )?;

        // Execute the gate
        let result = crate::gate::run_gate(gate_spec, &prep.worktree_path, None)?;

        // Parse the test summary from the gate output
        let test_summary =
            crate::gate::parse_cargo(&result.stdout).unwrap_or_else(|| crate::gate::TestSummary {
                passed: 0,
                failed: 0,
                ignored: 0,
                failures: vec![],
            });

        // Compute tree hash after gate
        let tree_hash_after =
            crate::git::git(&prep.worktree_path, &["rev-parse", "HEAD^{tree}"])?;

        // Record gate finished
        self.recorder.record(
            Some(task_id),
            crate::EventKind::GateFinished {
                attempt,
                gate_kind: gate,
                passed: result.passed,
                stdout: result.stdout.clone(),
                tree_hash: tree_hash_after,
            },
        )?;

        // Verify gate results based on phase type
        use crate::state::Phase;

        match spec.phase {
            Phase::Red => {
                let before_summary = before.ok_or_else(|| Error::Gate {
                    kind: "red".to_string(),
                    detail: "Red phase requires previous test summary".to_string(),
                })?;

                // Verify that red phase produces new failing tests
                let _newly_failing = crate::protocol::verify_red(before_summary, &test_summary)?;
            }
            Phase::Green => {
                let before_summary = before.ok_or_else(|| Error::Gate {
                    kind: "green".to_string(),
                    detail: "Green phase requires previous test summary".to_string(),
                })?;

                // Get the tests that should pass (from red phase failures)
                let expected_to_pass: Vec<String> = before_summary.failures.clone();

                // Verify that green phase passes the expected tests
                crate::protocol::verify_green(&expected_to_pass, &test_summary)?;
            }
            _ => {
                // Other phases don't have red/green gating logic
            }
        }

        Ok(test_summary)
    }

    /// Load the context document from the prompt library.
    ///
    /// Tries to load `context.md` from the global prompt library.
    /// Returns an empty string if the file doesn't exist.
    fn load_context_doc() -> Result<String> {
        let prompt_lib = crate::prompt_library()?;
        let context_path = prompt_lib.join("context.md");

        match std::fs::read_to_string(&context_path) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

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
    match RepoLock::acquire(&project.state_dir, Duration::from_secs(5)) {
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
    fn runner_new_requires_only_project() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let scenario_file = state_dir.join("scenario.toml");
        std::fs::write(&scenario_file, "steps = []").expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let runner = Runner::new(project).expect("Failed to create runner");

        assert_eq!(runner.project.id, "test-project");
        assert!(!runner.profile.gates.is_empty());
        assert_eq!(runner.provider.name(), "dummy");
    }

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

    #[test]
    fn runner_prepare_records_preflight_started() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        crate::git::git(repo.path(), &["push", "-u", "origin", "master"])
            .expect("Failed to push to origin");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");
        crate::git::git(repo.path(), &["push", "origin", "master"])
            .expect("Failed to push .gitignore");

        let scenario_file = state_dir.join("scenario.toml");
        std::fs::write(&scenario_file, "steps = []").expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);
        config.mainline_branch = "master".to_string();

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        let result = runner.prepare(&task);
        assert!(result.is_ok(), "prepare should succeed: {:?}", result.err());
    }

    #[test]
    fn runner_prepare_creates_worktree_from_fetched_sha() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        crate::git::git(repo.path(), &["push", "-u", "origin", "master"])
            .expect("Failed to push to origin");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");
        crate::git::git(repo.path(), &["push", "origin", "master"])
            .expect("Failed to push .gitignore");

        let scenario_file = state_dir.join("scenario.toml");
        std::fs::write(&scenario_file, "steps = []").expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);
        config.mainline_branch = "master".to_string();

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project.clone()).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        let prepared = runner.prepare(&task).expect("prepare should succeed");

        let worktree_path = &prepared.worktree_path;
        assert!(
            worktree_path.exists(),
            "worktree should exist at {worktree_path:?}",
        );

        let task_id = task.id;
        assert!(
            worktree_path.ends_with(format!("task-{task_id}")),
            "worktree path should end with task-{task_id}, got {worktree_path:?}",
        );

        let fetched_sha =
            crate::git::git(&project.root, &["rev-parse", "refs/remotes/origin/master"])
                .expect("Failed to get fetched SHA");

        let worktree_sha =
            crate::git::head_sha(worktree_path).expect("Failed to get worktree HEAD SHA");

        assert_eq!(
            worktree_sha, fetched_sha,
            "worktree should be checked out at fetched remote SHA"
        );
    }

    #[test]
    fn runner_prepare_holds_lock() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        crate::git::git(repo.path(), &["push", "-u", "origin", "master"])
            .expect("Failed to push to origin");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");
        crate::git::git(repo.path(), &["push", "origin", "master"])
            .expect("Failed to push .gitignore");

        let scenario_file = state_dir.join("scenario.toml");
        std::fs::write(&scenario_file, "steps = []").expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);
        config.mainline_branch = "master".to_string();

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project.clone()).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        let _prepared = runner.prepare(&task).expect("prepare should succeed");

        let lock_path = state_dir.join(".repo.lock");
        assert!(
            lock_path.exists(),
            "lock file should exist while prepared is held"
        );
    }

    #[test]
    fn runner_prepare_releases_lock_on_drop() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        crate::git::git(repo.path(), &["push", "-u", "origin", "master"])
            .expect("Failed to push to origin");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");
        crate::git::git(repo.path(), &["push", "origin", "master"])
            .expect("Failed to push .gitignore");

        let scenario_file = state_dir.join("scenario.toml");
        std::fs::write(&scenario_file, "steps = []").expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);
        config.mainline_branch = "master".to_string();

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project.clone()).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        {
            let _prepared = runner.prepare(&task).expect("prepare should succeed");
            let lock_path = state_dir.join(".repo.lock");
            assert!(lock_path.exists(), "lock file should exist");
        }

        let lock_path = state_dir.join(".repo.lock");
        assert!(
            !lock_path.exists(),
            "lock file should be released after prepared is dropped"
        );
    }

    #[test]
    fn runner_prepare_fails_when_baseline_gate_fails() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");

        let scenario_file = state_dir.join("scenario.toml");
        std::fs::write(&scenario_file, "steps = []").expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["false".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project.clone()).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        let result = runner.prepare(&task);
        assert!(
            result.is_err(),
            "prepare should fail when baseline gate fails"
        );
    }

    #[test]
    fn runner_run_phase_fails_when_report_is_missing() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        crate::git::git(repo.path(), &["push", "-u", "origin", "master"])
            .expect("Failed to push to origin");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");
        crate::git::git(repo.path(), &["push", "origin", "master"])
            .expect("Failed to push .gitignore");

        let scenario_file = state_dir.join("scenario.toml");
        // Create a scenario with a step that succeeds but doesn't write a report
        let scenario_toml = r#"
[[steps]]
outcome = "success"
stdout = "Task completed"
"#;
        std::fs::write(&scenario_file, scenario_toml).expect("Failed to write scenario file");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.dummy_scenario_path = Some(scenario_file);
        config.mainline_branch = "master".to_string();

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project.clone()).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        let prepared = runner.prepare(&task).expect("prepare should succeed");
        let spec = PhaseSpec {
            phase: crate::Phase::Implement,
            write_scope: crate::WriteScope::All,
            gate: None,
            records_evidence: true,
        };
        let attempt = AttemptId::new(1);

        let result = runner.run_phase(&prepared, &task, attempt, &spec);

        // The result should be a PhaseOutcome::Failure, not an error
        assert!(
            result.is_ok(),
            "run_phase should return a failure outcome, not an error: {result:?}"
        );
        let outcome = result.unwrap();
        match outcome {
            PhaseOutcome::Failure { class, detail } => {
                assert_eq!(class, FailureClass::VerificationFailure);
                assert!(detail.contains("did not produce a report"));
            }
            PhaseOutcome::Success => {
                panic!("run_phase should have failed due to missing report");
            }
        }
    }

    #[test]
    fn runner_run_phase_fails_when_write_scope_violated() {
        let repo = ScratchRepo::new().expect("Failed to create scratch repo");
        crate::git::git(repo.path(), &["push", "-u", "origin", "master"])
            .expect("Failed to push to origin");

        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");

        let gitignore = repo.path().join(".gitignore");
        std::fs::write(&gitignore, ".ktask/\n").expect("Failed to write .gitignore");
        crate::git::git(repo.path(), &["add", ".gitignore"]).expect("Failed to add .gitignore");
        crate::git::git(repo.path(), &["commit", "-m", "Add .gitignore"])
            .expect("Failed to commit .gitignore");
        crate::git::git(repo.path(), &["push", "origin", "master"])
            .expect("Failed to push .gitignore");

        let project = Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let mut config = Config::default();
        config.baseline_command = Some(vec!["true".to_string()]);
        config.verify_command = Some(vec!["true".to_string()]);
        config.mainline_branch = "master".to_string();

        // Create a scenario that writes the report
        let scenario_file = state_dir.join("scenario.toml");
        let scenario_toml = r#"
[[steps]]
outcome = "success"
stdout = "Task completed"
"#;
        std::fs::write(&scenario_file, scenario_toml).expect("Failed to write scenario file");
        config.dummy_scenario_path = Some(scenario_file);

        let config_path = crate::config::project_config_path(&project);
        let config_toml = toml::to_string_pretty(&config).expect("Failed to serialize config");
        std::fs::write(&config_path, config_toml).expect("Failed to write config");

        let mut runner = Runner::new(project.clone()).expect("Failed to create runner");

        let task = Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "test outcome".to_string(),
            done_when: "test done".to_string(),
            verify: "test verify".to_string(),
            refs: "test refs".to_string(),
        };

        let prepared = runner.prepare(&task).expect("prepare should succeed");

        // Manually create a forbidden file change in the worktree
        let forbidden_file = prepared.worktree_path.join("src").join("main.rs");
        std::fs::create_dir_all(forbidden_file.parent().unwrap())
            .expect("Failed to create src directory");
        std::fs::write(&forbidden_file, "// Modified\n").expect("Failed to write forbidden file");
        crate::git::git(&prepared.worktree_path, &["add", "src/main.rs"])
            .expect("Failed to stage file");
        crate::git::git(
            &prepared.worktree_path,
            &["commit", "-m", "Modified forbidden file"],
        )
        .expect("Failed to commit");

        // Create the report directory and file
        let report_path = crate::report_path(&project, task.id, AttemptId::new(1));
        if let Some(parent) = report_path.parent() {
            std::fs::create_dir_all(parent).expect("Failed to create report dir");
        }
        std::fs::write(&report_path, "KTASK_RESULT: DONE\n").expect("Failed to write report");

        // Use a read-only scope to trigger the violation
        let spec = PhaseSpec {
            phase: crate::Phase::Verify,
            write_scope: crate::WriteScope::None,
            gate: None,
            records_evidence: true,
        };
        let attempt = AttemptId::new(1);

        let result = runner.run_phase(&prepared, &task, attempt, &spec);

        // The result should be a PhaseOutcome::Failure
        assert!(
            result.is_ok(),
            "run_phase should return a failure outcome: {result:?}"
        );
        let outcome = result.unwrap();
        match outcome {
            PhaseOutcome::Failure { class, detail } => {
                assert_eq!(class, FailureClass::PolicyFailure);
                assert!(detail.contains("read-only") || detail.contains("not allowed"));
            }
            PhaseOutcome::Success => {
                panic!("run_phase should have failed due to write scope violation");
            }
        }
    }
}

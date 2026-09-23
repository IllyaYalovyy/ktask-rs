//! `preflight`: proving the world is sane before spending tokens
//! (`VISION.md` §6's `preflight` pipeline state).
//!
//! The rest of the supervisor loop this module is named for arrives in
//! later tasks; today it holds exactly the one entry point `preflight`
//! needs.

use std::collections::BTreeMap;
use std::time::Duration;

use nix::sys::statvfs::statvfs;

use crate::{
    Config, EventKind, FailureClass, Gate, GateKind, Invocation, Journal, Outcome, Project,
    Provider, Result, acquire, classify, fetch, head_sha, require_clean, run_gate,
};

/// One check in [`preflight`]'s fixed sequence: `Ok(())` when it passes, or
/// the [`FailureClass`] and human-readable detail to report when it does
/// not.
type CheckResult = std::result::Result<(), (FailureClass, String)>;

/// What [`preflight`] found: either every check passed, naming the commit
/// the task will be attempted from, or the first check that failed, naming
/// the [`FailureClass`] that should govern recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightReport {
    /// Every check passed.
    Passed {
        /// The commit the task will be attempted from: `project.root`'s
        /// `HEAD` once the mainline remote has been fetched and the
        /// working tree is confirmed clean.
        base_sha: String,
    },
    /// The first check that failed. Checks after it were never attempted.
    Failed {
        /// The class of failure, chosen the same way [`classify()`] would for
        /// the same underlying error where one exists (a git, policy or
        /// provider error), and [`FailureClass::EnvironmentFailure`] for a
        /// host-level condition `classify` has no vocabulary for (disk
        /// space, lock contention).
        class: FailureClass,
        /// A human-readable description of what failed.
        detail: String,
    },
}

/// Proves the world is sane before spending tokens (`VISION.md` §6):
/// `config.mainline_remote` fetches cleanly and `project.root`'s working
/// tree is clean, the baseline gate (if `config.baseline_command` names
/// one) is green, `provider` is available, free disk space is at or above
/// `config.min_free_disk_bytes`, and the repository lock can be acquired.
///
/// Checks run in that fixed order and stop at the first failure — later
/// checks are never attempted — mirroring [`crate::run_completion_set`]'s
/// own fail-fast behavior. A check that fails on its own terms (a red
/// baseline gate, a full disk, an unreachable provider) is not an `Err`
/// here: it is the [`PreflightReport::Failed`] this function exists to
/// produce, the same "ran and failed is not an error" convention
/// [`run_gate`] already follows. `Err` is reserved for preflight's own
/// plumbing failing: the journal could not be opened or written to.
///
/// Journals [`EventKind::PreflightStarted`] before any check runs, then
/// exactly one of [`EventKind::PreflightPassed`] or
/// [`EventKind::PreflightFailed`] once every check that ran has reported —
/// both with `task_id: None`, since preflight proves the repository itself
/// is sane, not any one task's fitness to run.
///
/// # Errors
///
/// Returns whatever [`crate::Error`] opening `project`'s journal or
/// appending to it produces.
pub fn preflight(
    project: &Project,
    config: &Config,
    provider: &dyn Provider,
) -> Result<PreflightReport> {
    let mut journal = Journal::open_for(project)?;
    journal.append(None, &EventKind::PreflightStarted)?;

    let report = run_checks(project, config, provider);

    let event = match &report {
        PreflightReport::Passed { base_sha } => EventKind::PreflightPassed {
            base_sha: base_sha.clone(),
        },
        PreflightReport::Failed { class, detail } => EventKind::PreflightFailed {
            class: *class,
            detail: detail.clone(),
        },
    };
    journal.append(None, &event)?;

    Ok(report)
}

/// Runs every check [`preflight`] documents, in order, stopping at the
/// first failure.
fn run_checks(project: &Project, config: &Config, provider: &dyn Provider) -> PreflightReport {
    if let Err((class, detail)) = check_remote_fetched(project, config) {
        return PreflightReport::Failed { class, detail };
    }
    if let Err((class, detail)) = check_mainline_clean(project) {
        return PreflightReport::Failed { class, detail };
    }
    if let Err((class, detail)) = check_baseline_gate(project, config) {
        return PreflightReport::Failed { class, detail };
    }
    if let Err((class, detail)) = check_provider_available(project, config, provider) {
        return PreflightReport::Failed { class, detail };
    }
    if let Err((class, detail)) = check_disk_space(project, config) {
        return PreflightReport::Failed { class, detail };
    }
    if let Err((class, detail)) = check_lock_acquirable(project) {
        return PreflightReport::Failed { class, detail };
    }

    match head_sha(&project.root) {
        Ok(base_sha) => PreflightReport::Passed { base_sha },
        Err(err) => PreflightReport::Failed {
            class: FailureClass::EnvironmentFailure,
            detail: err.to_string(),
        },
    }
}

/// A zeroed [`Outcome`] for the checks below that reuse [`classify()`] to
/// turn an [`crate::Error`] unrelated to any real attempt into a
/// [`FailureClass`], the same way `classify.rs`'s own tests do: only
/// `git_error` carries any information in these calls.
fn empty_outcome() -> Outcome {
    Outcome {
        exit_code: 1,
        stdout: String::new(),
        stderr: String::new(),
        usage: None,
        session_id: None,
    }
}

/// Checks that `config.mainline_remote` fetches cleanly into
/// `project.root`. A fetch failure is a `git` operation failing outside of
/// publication, which [`classify()`] (`VISION.md` §7) always reports as
/// [`FailureClass::GitConflict`].
fn check_remote_fetched(project: &Project, config: &Config) -> CheckResult {
    fetch(&project.root, &config.mainline_remote).map_err(|err| {
        let class = classify(&empty_outcome(), &[], Some(&err));
        (class, err.to_string())
    })
}

/// Checks that `project.root`'s working tree and index match `HEAD`.
/// Reuses [`classify()`] so a dirty tree reports
/// [`FailureClass::PolicyFailure`] exactly as `VISION.md` §7 names it, while
/// `project.root` somehow not being a git repository at all still reports
/// [`FailureClass::GitConflict`] rather than being misclassified as policy.
fn check_mainline_clean(project: &Project) -> CheckResult {
    require_clean(&project.root).map_err(|err| {
        let class = classify(&empty_outcome(), &[], Some(&err));
        (class, err.to_string())
    })
}

/// Checks that `config.baseline_command`, if configured, exits
/// successfully in `project.root`. A missing `baseline_command` is not a
/// failure: `Config`'s own doc comment says `None` "means the gate is not
/// configured," so there is nothing to prove.
fn check_baseline_gate(project: &Project, config: &Config) -> CheckResult {
    let Some(command) = &config.baseline_command else {
        return Ok(());
    };

    let gate = Gate {
        kind: GateKind::Baseline,
        command: command.clone(),
        timeout_secs: config.gate_timeout_secs,
        working_dir: None,
        env: BTreeMap::new(),
    };
    let result = run_gate(&gate, &project.root, None)
        .map_err(|err| (FailureClass::EnvironmentFailure, err.to_string()))?;

    if result.passed {
        Ok(())
    } else {
        let class = classify(&empty_outcome(), std::slice::from_ref(&result), None);
        let detail = format!(
            "baseline gate failed: exit {:?}{}",
            result.exit_code,
            if result.timed_out { " (timed out)" } else { "" }
        );
        Err((class, detail))
    }
}

/// Checks that `provider` is available by driving it through the one
/// operation [`Provider`] exposes for doing real work, [`Provider::invoke`],
/// with an empty prompt: the minimal invocation that still proves the
/// configured command can be spawned and authenticates, without asking the
/// provider to do anything. A provider that cannot even be reached reports
/// [`crate::Error::Provider`], which [`classify()`] resolves to
/// [`FailureClass::ProviderConfiguration`] or
/// [`FailureClass::ProviderTransient`] exactly as it would for a real
/// attempt's failed invocation.
fn check_provider_available(
    project: &Project,
    config: &Config,
    provider: &dyn Provider,
) -> CheckResult {
    let inv = Invocation {
        prompt: String::new(),
        model: config.model.clone(),
        working_dir: project.root.clone(),
    };
    provider.invoke(&inv, None).map(|_| ()).map_err(|err| {
        let class = classify(&empty_outcome(), &[], Some(&err));
        (class, err.to_string())
    })
}

/// Checks that `project.root`'s filesystem has at least
/// `config.min_free_disk_bytes` free, per `docs/DESIGN.md`'s note that
/// measuring free disk space needs a syscall with no safe `std` API — hence
/// `nix`'s `statvfs`, matching this crate's `unsafe_code = "forbid"`
/// (`docs/DESIGN.md` Dependencies). Neither failure mode fits `classify`'s
/// vocabulary of git, policy or provider errors, so both report
/// [`FailureClass::EnvironmentFailure`] directly: `classify.rs` itself names
/// "a full disk" as that class's example.
fn check_disk_space(project: &Project, config: &Config) -> CheckResult {
    let stat = statvfs(&project.root).map_err(|errno| {
        (
            FailureClass::EnvironmentFailure,
            format!(
                "could not read free disk space at {}: {errno}",
                project.root.display()
            ),
        )
    })?;
    let available = stat.blocks_available().saturating_mul(stat.fragment_size());

    if available < config.min_free_disk_bytes {
        Err((
            FailureClass::EnvironmentFailure,
            format!(
                "only {available} bytes free at {} ({} required)",
                project.root.display(),
                config.min_free_disk_bytes
            ),
        ))
    } else {
        Ok(())
    }
}

/// Checks that `project`'s repository lock ([`acquire`]) can be acquired
/// right now, releasing it immediately once proven: this is a check, not a
/// custody transfer, so nothing here holds the lock across the checks that
/// follow. A zero timeout makes the check synchronous — either the lock is
/// free, or a live holder is found and reported without waiting.
///
/// Lock contention is a host-level condition `classify` has no vocabulary
/// for (it would otherwise fall through every explicit case to
/// [`FailureClass::AgentFailure`], which is exactly backwards for a
/// condition the agent had nothing to do with), so this reports
/// [`FailureClass::EnvironmentFailure`] directly instead of going through
/// `classify`.
fn check_lock_acquirable(project: &Project) -> CheckResult {
    acquire(&project.state_dir, Duration::from_secs(0))
        .map(|_lock| ())
        .map_err(|err| (FailureClass::EnvironmentFailure, err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::scratch_repo;
    use crate::{Bus, Capabilities, Error};

    /// A [`Provider`] whose `invoke` always succeeds, proving
    /// [`check_provider_available`] (and the full [`preflight`] happy path)
    /// does not depend on a real Claude or Codex binary being installed.
    struct AlwaysAvailable;

    impl Provider for AlwaysAvailable {
        fn name(&self) -> &'static str {
            "always-available"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                structured_output: false,
                model_selection: false,
                usage_telemetry: false,
            }
        }

        fn invoke(&self, _inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
            Ok(Outcome {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                usage: None,
                session_id: None,
            })
        }
    }

    /// A [`Provider`] whose `invoke` always fails as a missing executable
    /// would, the exact shape `provider/claude.rs` documents.
    struct MissingExecutable;

    impl Provider for MissingExecutable {
        fn name(&self) -> &'static str {
            "missing-executable"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                structured_output: false,
                model_selection: false,
                usage_telemetry: false,
            }
        }

        fn invoke(&self, _inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
            Err(Error::Provider {
                provider: "missing-executable".to_string(),
                detail: "could not start `ktask-missing-binary`: No such file or directory \
                          (os error 2)"
                    .to_string(),
            })
        }
    }

    fn passing_config() -> Config {
        let mut config = Config::default();
        config.baseline_command = None;
        config.min_free_disk_bytes = 0;
        config
    }

    fn project_for(root: &std::path::Path, state_dir: &std::path::Path) -> Project {
        Project {
            root: root.to_path_buf(),
            id: "preflight-test".to_string(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn check_remote_fetched_passes_against_a_reachable_remote() {
        let repo = scratch_repo().expect("scratch_repo");
        let config = passing_config();
        let project = project_for(&repo.path, repo.path.as_path());

        check_remote_fetched(&project, &config).expect("fetch must succeed");
    }

    #[test]
    fn check_remote_fetched_fails_as_a_git_conflict_when_the_remote_is_unreachable() {
        let repo = scratch_repo().expect("scratch_repo");
        crate::git(
            &repo.path,
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/ktask-preflight-origin",
            ],
        )
        .expect("break the remote");
        let config = passing_config();
        let project = project_for(&repo.path, repo.path.as_path());

        let (class, detail) =
            check_remote_fetched(&project, &config).expect_err("unreachable remote must fail");

        assert_eq!(class, FailureClass::GitConflict);
        assert!(!detail.is_empty());
    }

    #[test]
    fn check_mainline_clean_passes_on_a_freshly_fetched_checkout() {
        let repo = scratch_repo().expect("scratch_repo");
        let project = project_for(&repo.path, repo.path.as_path());

        check_mainline_clean(&project).expect("a fresh checkout must be clean");
    }

    #[test]
    fn check_mainline_clean_fails_as_a_policy_failure_when_the_tree_is_dirty() {
        let repo = scratch_repo().expect("scratch_repo");
        std::fs::write(repo.path.join("untracked.txt"), "dirty\n").expect("write untracked file");
        let project = project_for(&repo.path, repo.path.as_path());

        let (class, detail) = check_mainline_clean(&project).expect_err("a dirty tree must fail");

        assert_eq!(class, FailureClass::PolicyFailure);
        assert!(detail.contains("untracked.txt"), "detail was: {detail}");
    }

    #[test]
    fn check_baseline_gate_passes_when_unconfigured() {
        let repo = scratch_repo().expect("scratch_repo");
        let config = passing_config();
        let project = project_for(&repo.path, repo.path.as_path());

        check_baseline_gate(&project, &config).expect("an unconfigured baseline is not a failure");
    }

    #[test]
    fn check_baseline_gate_passes_when_the_command_succeeds() {
        let repo = scratch_repo().expect("scratch_repo");
        let mut config = passing_config();
        config.baseline_command = Some(vec!["true".to_string()]);
        let project = project_for(&repo.path, repo.path.as_path());

        check_baseline_gate(&project, &config).expect("a passing baseline command must pass");
    }

    #[test]
    fn check_baseline_gate_fails_as_a_verification_failure_when_the_command_exits_nonzero() {
        let repo = scratch_repo().expect("scratch_repo");
        let mut config = passing_config();
        config.baseline_command = Some(vec!["false".to_string()]);
        let project = project_for(&repo.path, repo.path.as_path());

        let (class, _detail) =
            check_baseline_gate(&project, &config).expect_err("a failing baseline must fail");

        assert_eq!(class, FailureClass::VerificationFailure);
    }

    #[test]
    fn check_baseline_gate_fails_as_an_environment_failure_when_the_command_cannot_be_spawned() {
        let repo = scratch_repo().expect("scratch_repo");
        let mut config = passing_config();
        config.baseline_command = Some(vec!["ktask-preflight-nonexistent-binary".to_string()]);
        let project = project_for(&repo.path, repo.path.as_path());

        let (class, _detail) = check_baseline_gate(&project, &config)
            .expect_err("an unspawnable baseline command must fail");

        assert_eq!(class, FailureClass::EnvironmentFailure);
    }

    #[test]
    fn check_provider_available_passes_for_an_available_provider() {
        let repo = scratch_repo().expect("scratch_repo");
        let config = passing_config();
        let project = project_for(&repo.path, repo.path.as_path());

        check_provider_available(&project, &config, &AlwaysAvailable)
            .expect("an available provider must pass");
    }

    #[test]
    fn check_provider_available_fails_as_provider_configuration_for_a_missing_executable() {
        let repo = scratch_repo().expect("scratch_repo");
        let config = passing_config();
        let project = project_for(&repo.path, repo.path.as_path());

        let (class, detail) = check_provider_available(&project, &config, &MissingExecutable)
            .expect_err("a missing executable must fail");

        assert_eq!(class, FailureClass::ProviderConfiguration);
        assert!(
            detail.contains("ktask-missing-binary"),
            "detail was: {detail}"
        );
    }

    #[test]
    fn check_disk_space_passes_when_the_threshold_is_trivially_low() {
        let repo = scratch_repo().expect("scratch_repo");
        let config = passing_config();
        let project = project_for(&repo.path, repo.path.as_path());

        check_disk_space(&project, &config).expect("a zero threshold must always pass");
    }

    #[test]
    fn check_disk_space_fails_as_an_environment_failure_when_the_threshold_is_unmeetable() {
        let repo = scratch_repo().expect("scratch_repo");
        let mut config = passing_config();
        config.min_free_disk_bytes = u64::MAX;
        let project = project_for(&repo.path, repo.path.as_path());

        let (class, detail) =
            check_disk_space(&project, &config).expect_err("an impossible threshold must fail");

        assert_eq!(class, FailureClass::EnvironmentFailure);
        assert!(!detail.is_empty());
    }

    #[test]
    fn check_lock_acquirable_passes_when_no_lock_is_held() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());

        check_lock_acquirable(&project).expect("an unlocked repository must pass");
    }

    #[test]
    fn check_lock_acquirable_fails_as_an_environment_failure_when_already_held() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let _held = acquire(state_dir.path(), Duration::from_secs(30)).expect("hold the lock");

        let (class, detail) =
            check_lock_acquirable(&project).expect_err("an already-held lock must fail");

        assert_eq!(class, FailureClass::EnvironmentFailure);
        assert!(!detail.is_empty());
    }

    #[test]
    fn preflight_passes_and_journals_the_base_sha_when_every_check_succeeds() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let config = passing_config();
        let project = project_for(&repo.path, state_dir.path());

        let report =
            preflight(&project, &config, &AlwaysAvailable).expect("preflight must not error");

        assert_eq!(
            report,
            PreflightReport::Passed {
                base_sha: repo.seed_sha.clone(),
            }
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events().expect("read events");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event.kind.discriminant())
            .collect();
        assert_eq!(kinds, vec!["PreflightStarted", "PreflightPassed"]);
        assert!(events.iter().all(|event| event.task_id.is_none()));
    }

    #[test]
    fn preflight_fails_and_journals_the_failure_class_when_a_check_fails() {
        let repo = scratch_repo().expect("scratch_repo");
        std::fs::write(repo.path.join("untracked.txt"), "dirty\n").expect("write untracked file");
        let state_dir = tempfile::tempdir().expect("state dir");
        let config = passing_config();
        let project = project_for(&repo.path, state_dir.path());

        let report =
            preflight(&project, &config, &AlwaysAvailable).expect("preflight must not error");

        let PreflightReport::Failed { class, .. } = report else {
            panic!("expected a failed preflight, got {report:?}");
        };
        assert_eq!(class, FailureClass::PolicyFailure);

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events().expect("read events");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event.kind.discriminant())
            .collect();
        assert_eq!(kinds, vec!["PreflightStarted", "PreflightFailed"]);
    }
}

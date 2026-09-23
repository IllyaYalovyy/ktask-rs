//! `preflight` and `Runner`: proving the world is sane before spending
//! tokens, then opening the first attempt against it (`VISION.md` §6).
//!
//! The rest of the supervisor loop this module is named for arrives in
//! later tasks; today it holds `preflight` and `Runner`'s construction and
//! `begin_attempt`, the entry points those tasks have needed so far.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

use nix::sys::statvfs::statvfs;
use time::OffsetDateTime;

use crate::{
    AttemptId, AttemptRecord, Bus, Config, Error, EventKind, FailureClass, Gate, GateKind,
    Invocation, Journal, Outcome, PhaseSpec, Profile, Project, Provider, Recorder, RepoLock,
    ReportResult, Result, Stream, Task, TaskId, acquire, assemble, build, changed_paths,
    check_model, check_scope, classify, collect_adrs, create_worktree, ensure_report_dir, fetch,
    for_task, head_sha, load, load_context_doc, load_for, load_template, profile_from, read_report,
    require_clean, run_gate, write_evidence,
};

/// How long [`Runner::prepare`] waits for the repository lock ([`acquire`])
/// to become free before giving up. [`preflight`]'s own last check has
/// already confirmed no live holder was found a moment earlier, so this only
/// has to cover the narrow window between that check and this acquisition.
const PREPARE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// The supervisor for one project: everything an attempt runs against, held
/// together so nothing that drives an attempt has to reassemble it from
/// scratch.
///
/// Holds the effective [`Config`] a task's protocol and gates are resolved
/// against, the [`Profile`] of gates [`Config`] implies, the [`Recorder`]
/// through which every event this project's runs produce is journaled and
/// published, and the [`Provider`] driving its agent.
pub struct Runner {
    /// The project this runner drives attempts against.
    project: Project,
    /// This project's effective configuration.
    config: Config,
    /// The verification profile `config` implies.
    profile: Profile,
    /// Where every event this runner produces is journaled and published.
    recorder: Recorder,
    /// The agent backend attempts are driven through.
    provider: Box<dyn Provider>,
}

/// Manual, since [`Provider`] (unlike every other field here) does not
/// itself implement [`std::fmt::Debug`]; `provider`'s name stands in for it.
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
    /// Builds a `Runner` for `project`.
    ///
    /// Opens `project`'s journal ([`Journal::open_for`]), loads its
    /// effective configuration ([`load_for`]), and derives everything else
    /// from that config alone: the gate profile ([`profile_from`]), the
    /// provider it names ([`build`]), and a [`Bus`] sized to
    /// `config.output_ring_lines` — the same capacity `docs/DESIGN.md`
    /// documents for every subscriber's ring — wrapped with the journal into
    /// a [`Recorder`] so nothing downstream can append to the journal
    /// without also publishing, or vice versa.
    ///
    /// Nothing beyond `project` itself is required: no attempt, no task, no
    /// live subscriber has to already exist. `project` must already be
    /// registered ([`Project::register`]), since that is what creates its
    /// state directory and journal in the first place.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if `project`'s
    /// journal cannot be opened, [`Error::Config`] if
    /// its configuration cannot be loaded, if the resulting profile has no
    /// `verify_command`, or if `config.provider` cannot be built, and
    /// whatever else [`load_for`] or [`build`] themselves return.
    pub fn new(project: Project) -> Result<Runner> {
        let journal = Journal::open_for(&project)?;
        let config = load_for(&project)?;
        let bus = Bus::new(usize::try_from(config.output_ring_lines).unwrap_or(usize::MAX));
        let recorder = Recorder::new(journal, bus);
        let profile = profile_from(&config)?;
        let provider = build(&config)?;

        Ok(Runner {
            project,
            config,
            profile,
            recorder,
            provider,
        })
    }

    /// Opens a new attempt at `task`.
    ///
    /// Resolves `task`'s protocol ([`for_task`]), reads `project.root`'s
    /// current commit as the attempt's base SHA, and records
    /// [`EventKind::AttemptStarted`] carrying that protocol's name, this
    /// process's pid and the base SHA — then immediately persists the
    /// attempt's evidence directory ([`write_evidence`]), so an attempt's
    /// evidence exists from the moment it starts rather than only once it
    /// ends (`VISION.md` §6).
    ///
    /// The returned [`AttemptId`] is one past the highest attempt already
    /// recorded for `task` in the journal, or `1` if `task` has none yet.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Policy`] if `task`'s protocol
    /// cannot be resolved, whatever [`head_sha`] returns if `project.root`'s
    /// current commit cannot be read, and whatever [`Recorder::record`] or
    /// [`write_evidence`] return on failure to persist.
    pub fn begin_attempt(&mut self, task: &Task) -> Result<AttemptId> {
        let attempt = next_attempt_id(&self.project, task.id)?;
        let protocol = for_task(task, &self.config)?;
        let base_sha = head_sha(&self.project.root)?;
        let pid = process::id();

        self.recorder.record(
            Some(task.id),
            EventKind::AttemptStarted {
                attempt,
                protocol: protocol.name.to_string(),
                pid,
                base_sha: base_sha.clone(),
            },
        )?;

        let record = AttemptRecord {
            id: attempt,
            task: task.id,
            started: OffsetDateTime::now_utc(),
            ended: None,
            model_configured: self.config.model.clone(),
            model_reported: None,
            session_id: None,
            exit_reason: "in_progress".to_string(),
            gates: Vec::new(),
            usage: None,
            base_sha,
            candidate_sha: None,
        };
        write_evidence(&self.project, &record, "")?;

        Ok(attempt)
    }

    /// Creates the directory `task`'s `attempt` report will be written to
    /// ([`crate::report_path`]) before any provider that might write into it
    /// starts, and returns the prompt fragment naming that exact path: a
    /// caller assembling the full prompt for this attempt includes it, so
    /// the agent is told exactly where its report belongs rather than
    /// having to guess a convention.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the report's directory cannot be created.
    pub fn name_report_path(&self, task: &Task, attempt: AttemptId) -> Result<String> {
        let path = ensure_report_dir(&self.project, task.id, attempt)?;
        Ok(format!(
            "Your task report should be written to `{}` before exiting.\n",
            path.display()
        ))
    }

    /// Reads back and parses the report [`Runner::name_report_path`] told
    /// the provider to write, once it has exited (`VISION.md` §3 invariant
    /// 4: "a task is never done based only on an agent exit code or
    /// statement"). Delegates entirely to [`crate::read_report`], which
    /// draws the same [`crate::report_path`] this attempt's directory was
    /// created at, so a report from a different attempt is never mistaken
    /// for this one's.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Report`] naming the expected path if the provider
    /// never wrote a report there, or if what it wrote does not parse.
    pub fn collect_report(&self, task: &Task, attempt: AttemptId) -> Result<ReportResult> {
        read_report(&self.project, task.id, attempt)
    }

    /// Brings `task` to the point an agent could start (`VISION.md` §6, §10):
    /// proves the world is sane ([`preflight`]), serializes against every
    /// other process working this repository ([`acquire`]), and creates
    /// `task`'s isolated worktree from the commit preflight just proved was
    /// freshly fetched ([`create_worktree`]).
    ///
    /// Unlike the project-wide [`preflight`] itself, `prepare` journals
    /// `task`'s own [`EventKind::PreflightStarted`] before calling it and
    /// `task`'s own [`EventKind::PreflightPassed`] or
    /// [`EventKind::PreflightFailed`] once it returns, so `task`'s pipeline
    /// state (`VISION.md` §6) actually advances into `Preflight` rather than
    /// preflight remaining an event only the repository as a whole
    /// experienced.
    ///
    /// The repository lock is only acquired once preflight has reported
    /// success: a failed preflight never held it, so there is nothing to
    /// release on that path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Preflight`] carrying the failed check's
    /// [`FailureClass`] and detail when preflight itself reports
    /// [`PreflightReport::Failed`]; whatever [`Recorder::record`] returns on
    /// failure to journal; [`Error::LockTimeout`] if the repository lock
    /// cannot be acquired within a bounded timeout; and whatever
    /// [`create_worktree`] returns if the worktree cannot be created.
    pub fn prepare(&mut self, task: &Task) -> Result<Prepared> {
        self.recorder
            .record(Some(task.id), EventKind::PreflightStarted)?;

        let report = preflight(&self.project, &self.config, self.provider.as_ref())?;

        let base_sha = match report {
            PreflightReport::Passed { base_sha } => {
                self.recorder.record(
                    Some(task.id),
                    EventKind::PreflightPassed {
                        base_sha: base_sha.clone(),
                    },
                )?;
                base_sha
            }
            PreflightReport::Failed { class, detail } => {
                self.recorder.record(
                    Some(task.id),
                    EventKind::PreflightFailed {
                        class,
                        detail: detail.clone(),
                    },
                )?;
                return Err(Error::Preflight { class, detail });
            }
        };

        let lock = acquire(&self.project.state_dir, PREPARE_LOCK_TIMEOUT)?;
        let worktree =
            create_worktree(&self.project.root, &format!("task-{}", task.id), &base_sha)?;

        Ok(Prepared {
            worktree,
            base_sha,
            lock,
        })
    }

    /// Runs one phase of `task`'s protocol for `attempt` (`VISION.md` §9):
    /// enters the phase, assembles the prompt, invokes the provider,
    /// records what happened, and confirms the agent stayed within
    /// `spec.write_scope`.
    ///
    /// In order:
    ///
    /// 1. Records [`EventKind::PhaseEntered`].
    /// 2. Assembles the prompt from [`crate::assemble`] — the static
    ///    context document ([`crate::load_context_doc`]), every recorded
    ///    ADR ([`crate::collect_adrs`]) and the task template
    ///    ([`crate::load_template`]) — plus [`Runner::name_report_path`]'s
    ///    fragment naming exactly where the agent's report belongs, which
    ///    also creates that report's directory as a side effect.
    /// 3. Invokes `self.provider` in `prep.worktree`.
    /// 4. Checks the configured model against whatever the provider
    ///    reported ([`crate::check_model`]) before anything from this
    ///    invocation is journaled, so a mismatched model is rejected
    ///    outright rather than also being recorded as if it were accepted.
    /// 5. Records [`EventKind::AgentOutput`] for whichever of stdout/stderr
    ///    the provider produced, then [`EventKind::AttemptFinished`].
    /// 6. Reads back the report the agent was told to write
    ///    ([`crate::read_report`]) — a missing report is a classified
    ///    failure, never an assumed success (`VISION.md` §3 invariant 4).
    /// 7. Confirms every path changed since `prep.base_sha`
    ///    ([`crate::changed_paths`]) falls within `spec.write_scope`
    ///    ([`crate::check_scope`]). A well-behaved provider commits its own
    ///    work as part of running; a test double that only writes files
    ///    without committing produces no diff for this step to see.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Recorder::record`] returns on failure to journal;
    /// whatever `self.provider.invoke` returns if it cannot be invoked at
    /// all; [`Error::Provider`] if the configured and reported models
    /// mismatch; [`Error::Report`] naming the expected path if the agent
    /// never wrote a report there, or if what it wrote does not parse; and
    /// [`Error::Policy`] naming every path `spec.write_scope` did not
    /// permit.
    pub fn run_phase(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        spec: &PhaseSpec,
    ) -> Result<PhaseOutcome> {
        self.recorder.record(
            Some(task.id),
            EventKind::PhaseEntered {
                attempt,
                phase: spec.phase,
            },
        )?;

        let context_doc = load_context_doc(&self.project)?;
        let adrs = collect_adrs(&self.project.root)?;
        let template = load_template(&self.project)?;
        let total = load(&self.project)?.len();
        let mut prompt = assemble(task, &context_doc, &adrs, &template, attempt, total);
        prompt.push_str(&self.name_report_path(task, attempt)?);

        let inv = Invocation {
            prompt,
            model: self.config.model.clone(),
            working_dir: prep.worktree.clone(),
        };
        let outcome = self.provider.invoke(&inv, None)?;

        check_model(self.config.model.as_deref(), None)?;

        if !outcome.stdout.is_empty() {
            self.recorder.record(
                Some(task.id),
                EventKind::AgentOutput {
                    attempt,
                    stream: Stream::Stdout,
                    text: outcome.stdout.clone(),
                },
            )?;
        }
        if !outcome.stderr.is_empty() {
            self.recorder.record(
                Some(task.id),
                EventKind::AgentOutput {
                    attempt,
                    stream: Stream::Stderr,
                    text: outcome.stderr.clone(),
                },
            )?;
        }
        self.recorder.record(
            Some(task.id),
            EventKind::AttemptFinished {
                attempt,
                exit_code: outcome.exit_code,
                usage: outcome.usage,
                session_id: outcome.session_id.clone(),
                model_reported: None,
            },
        )?;

        let report = read_report(&self.project, task.id, attempt)?;

        let changed = changed_paths(&prep.worktree, &prep.base_sha)?;
        check_scope(spec.write_scope, &changed, &self.config.test_globs)?;

        Ok(PhaseOutcome { report, changed })
    }
}

/// What one call to [`Runner::run_phase`] produced: the agent's own claim
/// about the phase ([`ReportResult`]) and every path it actually changed,
/// already confirmed to fall within the phase's write scope
/// ([`crate::check_scope`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseOutcome {
    /// What the agent's report claimed.
    pub report: ReportResult,
    /// Every path changed since the attempt's base commit, already checked
    /// against the phase's write scope.
    pub changed: Vec<PathBuf>,
}

/// Everything an attempt needs to actually start: the isolated worktree
/// [`Runner::prepare`] created for a task, the commit it was created from,
/// and the repository lock serializing this run against every other process
/// working the same repository.
///
/// Dropping `Prepared` releases the repository lock, since [`RepoLock`]'s
/// own `Drop` impl is what does that — nothing here has to remember to
/// release it explicitly.
#[derive(Debug)]
pub struct Prepared {
    /// The task's isolated worktree, checked out at `base_sha`.
    pub worktree: PathBuf,
    /// The commit `worktree` was created from: the mainline remote's tip
    /// once [`preflight`] confirmed it was freshly fetched.
    pub base_sha: String,
    /// The repository lock, held for as long as `Prepared` lives.
    pub lock: RepoLock,
}

/// The next [`AttemptId`] for `task`: one past the highest `attempt` any
/// already-journaled [`EventKind::AttemptStarted`] names for it, or `1` if
/// none has run yet.
///
/// Opens its own read of `project`'s journal rather than going through a
/// [`Runner`]'s [`Recorder`], which exposes no way to read events back —
/// the same read-only access [`crate::read_evidence`] already relies on for
/// the same journal file.
///
/// # Errors
///
/// Returns whatever [`Journal::open_for`] or [`Journal::events_for`] return
/// on failure to open or read the journal.
fn next_attempt_id(project: &Project, task: TaskId) -> Result<AttemptId> {
    let journal = Journal::open_for(project)?;
    let last = journal
        .events_for(task)?
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::AttemptStarted { attempt, .. } => Some(attempt.get()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    Ok(AttemptId::new(last + 1))
}

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
    use crate::{
        Bus, Capabilities, Error, Phase, Scenario, ScenarioFile, Step, StepOutcome, TaskStatus,
        WriteScope, project_config_path, read_evidence, report_path,
    };

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

    /// Writes a project config naming `"codex"` as the provider (which,
    /// unlike the default `"dummy"`, needs no scenario file and never
    /// touches a real binary until `Provider::invoke` is actually called)
    /// and a trivial `verify_command`, the one gate [`profile_from`]
    /// requires: together, the minimum a project's own config needs for
    /// [`Runner::new`] to succeed without depending on any global config
    /// file the machine running this test happens to have.
    fn write_runnable_config(project: &Project) {
        std::fs::write(
            project_config_path(project),
            "provider = \"codex\"\nverify_command = [\"true\"]\n",
        )
        .expect("write project config");
    }

    fn sample_task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: "Do the thing".to_string(),
            outcome: "the thing is done".to_string(),
            done_when: "it is done".to_string(),
            verify: "true".to_string(),
            refs: String::new(),
            protocol: None,
        }
    }

    #[test]
    fn new_requires_nothing_but_a_registered_project() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);

        Runner::new(project).expect("a project with a runnable config must build a Runner");
    }

    #[test]
    fn begin_attempt_records_exactly_one_attempt_started_event_carrying_pid_and_base_sha() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let attempt = runner.begin_attempt(&task).expect("begin_attempt");
        assert_eq!(attempt, AttemptId::new(1));

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        assert_eq!(events.len(), 1, "exactly one event must be journaled");

        let kind = events[0].kind.clone();
        let EventKind::AttemptStarted {
            attempt: recorded_attempt,
            protocol,
            pid,
            base_sha,
        } = kind
        else {
            panic!("expected AttemptStarted, got {kind:?}");
        };
        assert_eq!(recorded_attempt, attempt);
        assert_eq!(protocol, "direct");
        assert_eq!(pid, process::id());
        assert_eq!(base_sha, repo.seed_sha);
    }

    #[test]
    fn begin_attempt_writes_evidence_from_the_moment_it_starts_not_only_once_it_ends() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let attempt = runner.begin_attempt(&task).expect("begin_attempt");

        let evidence = read_evidence(&project, task.id).expect("read_evidence");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].id, attempt);
        assert_eq!(evidence[0].ended, None);
        assert_eq!(evidence[0].base_sha, repo.seed_sha);
    }

    #[test]
    fn begin_attempt_allocates_increasing_attempt_ids_across_retries() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let first = runner.begin_attempt(&task).expect("first attempt");
        let second = runner.begin_attempt(&task).expect("second attempt");

        assert_eq!(first, AttemptId::new(1));
        assert_eq!(second, AttemptId::new(2));
    }

    #[test]
    fn name_report_path_creates_the_directory_and_names_the_exact_report_path() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let runner = Runner::new(project.clone()).expect("build runner");
        let named = runner
            .name_report_path(&task, attempt)
            .expect("name_report_path");

        let expected = report_path(&project, task.id, attempt);
        assert!(
            expected.parent().expect("report.md has a parent").is_dir(),
            "the report's directory must exist before a provider could run"
        );
        assert!(
            named.contains(&expected.display().to_string()),
            "prompt fragment must name the exact report path, got: {named:?}"
        );
    }

    #[test]
    fn collect_report_round_trips_a_report_the_provider_wrote_at_the_named_path() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let runner = Runner::new(project.clone()).expect("build runner");
        runner
            .name_report_path(&task, attempt)
            .expect("name_report_path");
        std::fs::write(
            report_path(&project, task.id, attempt),
            "KTASK_RESULT: DONE\nSummary: it worked.\n",
        )
        .expect("simulate the provider writing its report");

        let result = runner
            .collect_report(&task, attempt)
            .expect("collect_report");

        assert_eq!(result, ReportResult::Done);
    }

    #[test]
    fn collect_report_for_a_missing_report_is_a_classified_failure_naming_the_expected_path() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let runner = Runner::new(project.clone()).expect("build runner");
        runner
            .name_report_path(&task, attempt)
            .expect("name_report_path");

        let err = runner
            .collect_report(&task, attempt)
            .expect_err("the provider never wrote a report");

        assert!(matches!(err, Error::Report { .. }));
        let expected = report_path(&project, task.id, attempt);
        assert!(
            err.to_string().contains(&expected.display().to_string()),
            "error must name the expected path, got: {err}"
        );
    }

    #[test]
    fn collect_report_never_mistakes_a_previous_attempts_report_for_the_current_ones() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let first = runner.begin_attempt(&task).expect("first attempt");
        runner
            .name_report_path(&task, first)
            .expect("name_report_path for first attempt");
        std::fs::write(
            report_path(&project, task.id, first),
            "KTASK_RESULT: DONE\nSummary: first attempt worked.\n",
        )
        .expect("write first attempt's report");

        let second = runner.begin_attempt(&task).expect("second attempt");
        runner
            .name_report_path(&task, second)
            .expect("name_report_path for second attempt");

        // The second attempt's own report was never written: it must not
        // be mistaken for the first attempt's `Done` report.
        let err = runner
            .collect_report(&task, second)
            .expect_err("the second attempt never wrote its own report");
        assert!(matches!(err, Error::Report { .. }));

        // The first attempt's report is untouched by the second attempt's
        // directory having been created.
        let first_result = runner
            .collect_report(&task, first)
            .expect("collect first attempt's report");
        assert_eq!(first_result, ReportResult::Done);
    }

    #[test]
    fn new_debug_formats_the_provider_by_name_rather_than_requiring_provider_to_implement_debug() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);

        let runner = Runner::new(project).expect("build runner");

        let debugged = format!("{runner:?}");
        assert!(debugged.contains("Runner"));
        assert!(debugged.contains("codex"), "debugged was: {debugged}");
    }

    #[test]
    fn next_attempt_id_ignores_task_events_that_are_not_attempt_started() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);

        {
            let mut journal = Journal::open_for(&project).expect("open journal");
            journal
                .append(
                    Some(task.id),
                    &EventKind::TaskQueued {
                        title: "Add widget".to_string(),
                    },
                )
                .expect("append an unrelated task event");
        }

        let next = next_attempt_id(&project, task.id).expect("next_attempt_id");
        assert_eq!(
            next,
            AttemptId::new(1),
            "a non-AttemptStarted event must not be mistaken for a prior attempt"
        );
    }

    #[test]
    fn begin_attempt_resolves_the_tdd_protocol_when_the_task_names_it() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let mut task = sample_task(1);
        task.protocol = Some("tdd".to_string());

        let mut runner = Runner::new(project.clone()).expect("build runner");
        runner.begin_attempt(&task).expect("begin_attempt");

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let EventKind::AttemptStarted { protocol, .. } = &events[0].kind else {
            panic!("expected AttemptStarted, got {:?}", events[0].kind);
        };
        assert_eq!(protocol, "tdd");
    }

    /// Writes a project config naming the `dummy` provider with a scenario
    /// containing exactly one `success` step, plus the trivial
    /// `verify_command` `profile_from` requires: enough for `Runner::new` to
    /// build, and for `preflight`'s `check_provider_available` to succeed by
    /// actually invoking the provider, without any real agent binary
    /// installed (`VISION.md` §12).
    fn write_config_with_an_available_dummy_provider(
        project: &Project,
        scenario_dir: &std::path::Path,
    ) {
        let scenario_path = scenario_dir.join("scenario.toml");
        std::fs::write(&scenario_path, "[[steps]]\noutcome = \"success\"\n")
            .expect("write scenario file");
        std::fs::write(
            project_config_path(project),
            format!(
                "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = [\"true\"]\n",
                scenario_path.display()
            ),
        )
        .expect("write project config");
    }

    #[test]
    fn prepare_creates_a_worktree_checked_out_at_the_fetched_remote_sha_and_holds_the_lock() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_config_with_an_available_dummy_provider(&project, state_dir.path());
        let task = sample_task(1);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let prepared = runner.prepare(&task).expect("prepare");

        assert_eq!(prepared.base_sha, repo.seed_sha);
        assert!(
            prepared.worktree.is_dir(),
            "worktree must actually exist on disk"
        );
        assert_eq!(
            head_sha(&prepared.worktree).expect("head_sha of worktree"),
            repo.seed_sha,
            "the worktree must be checked out at the fetched remote sha, not merely named after it"
        );

        let contended = acquire(&project.state_dir, Duration::from_secs(0))
            .expect_err("the repository lock must already be held while `prepared` is alive");
        assert!(matches!(contended, Error::LockTimeout { .. }));

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event.kind.discriminant())
            .collect();
        assert_eq!(kinds, vec!["PreflightStarted", "PreflightPassed"]);

        let worktree = prepared.worktree.clone();
        drop(prepared);
        drop(
            acquire(&project.state_dir, Duration::from_secs(0))
                .expect("dropping `Prepared` must release the repository lock"),
        );
        crate::remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn prepare_never_acquires_the_lock_when_preflight_fails() {
        let repo = scratch_repo().expect("scratch_repo");
        std::fs::write(repo.path.join("untracked.txt"), "dirty\n").expect("write untracked file");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_config_with_an_available_dummy_provider(&project, state_dir.path());
        let task = sample_task(1);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let err = runner
            .prepare(&task)
            .expect_err("a dirty tree must fail preflight");

        match err {
            Error::Preflight { class, .. } => assert_eq!(class, FailureClass::PolicyFailure),
            other => panic!("expected Error::Preflight, got {other:?}"),
        }

        drop(
            acquire(&project.state_dir, Duration::from_secs(0))
                .expect("a failed preflight must not leave the repository lock held"),
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event.kind.discriminant())
            .collect();
        assert_eq!(kinds, vec!["PreflightStarted", "PreflightFailed"]);
    }

    fn implement_spec() -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Implement,
            write_scope: WriteScope::All,
            gate: None,
            records_evidence: true,
        }
    }

    fn verify_spec() -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Verify,
            write_scope: WriteScope::None,
            gate: None,
            records_evidence: true,
        }
    }

    /// A [`Config`] with a `verify_command` set, the one thing
    /// [`write_config_with_an_available_dummy_provider`]'s TOML config
    /// carries that a bare [`Config::default`] does not: `run_phase`'s own
    /// tests build a [`Runner`] by hand (to swap in a provider `build`
    /// cannot construct), bypassing [`load_for`] entirely.
    fn runnable_config() -> Config {
        let mut config = passing_config();
        config.verify_command = Some(vec!["true".to_string()]);
        config
    }

    /// Builds a [`Runner`] directly from its fields rather than through
    /// [`Runner::new`], so a test can drive it with a [`Provider`]
    /// [`build`] has no way to construct (`run_phase`'s scope-violation
    /// test needs a provider that also commits, which no built-in adapter
    /// under test does).
    fn manual_runner(project: &Project, config: Config, provider: Box<dyn Provider>) -> Runner {
        let journal = Journal::open_for(project).expect("open journal");
        let bus = Bus::new(usize::try_from(config.output_ring_lines).unwrap_or(usize::MAX));
        let recorder = Recorder::new(journal, bus);
        let profile = profile_from(&config).expect("profile_from");
        Runner {
            project: project.clone(),
            config,
            profile,
            recorder,
            provider,
        }
    }

    #[test]
    fn run_phase_records_phase_entered_agent_output_and_attempt_finished_then_returns_the_parsed_report()
     {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);
        let expected_report_path = report_path(&project, task.id, attempt);

        let scenario = Scenario {
            steps: vec![
                // Consumed by `prepare`'s own `check_provider_available`.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: Vec::new(),
                },
                // Consumed by `run_phase`'s own invocation.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("implemented the thing\n".to_string()),
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: expected_report_path.clone(),
                        content: "KTASK_RESULT: DONE\nSummary: it worked.\n".to_string(),
                    }],
                },
            ],
        };
        let scenario_path = state_dir.path().join("scenario.toml");
        std::fs::write(&scenario_path, scenario.to_toml().expect("serialize"))
            .expect("write scenario");
        std::fs::write(
            project_config_path(&project),
            format!(
                "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = [\"true\"]\n",
                scenario_path.display()
            ),
        )
        .expect("write project config");

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let prep = runner.prepare(&task).expect("prepare");

        let outcome = runner
            .run_phase(&prep, &task, attempt, &implement_spec())
            .expect("run_phase");

        assert_eq!(outcome.report, ReportResult::Done);
        assert!(
            outcome.changed.is_empty(),
            "the dummy provider never commits, so there is nothing to diff: {:?}",
            outcome.changed
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event.kind.discriminant())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "PhaseEntered",
                "AgentOutput",
                "AttemptFinished",
            ]
        );

        let EventKind::AttemptFinished { exit_code, .. } = &events[4].kind else {
            panic!("expected AttemptFinished, got {:?}", events[4].kind);
        };
        assert_eq!(*exit_code, 0);

        let worktree = prep.worktree.clone();
        drop(prep);
        crate::remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn run_phase_fails_with_a_classified_report_error_when_the_agent_never_writes_one() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);
        let expected_report_path = report_path(&project, task.id, attempt);

        // Two steps, neither of which writes a report file: one for
        // `prepare`'s own availability check, one for `run_phase`'s.
        let scenario = Scenario {
            steps: vec![
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: Vec::new(),
                },
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: Vec::new(),
                },
            ],
        };
        let scenario_path = state_dir.path().join("scenario.toml");
        std::fs::write(&scenario_path, scenario.to_toml().expect("serialize"))
            .expect("write scenario");
        std::fs::write(
            project_config_path(&project),
            format!(
                "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = [\"true\"]\n",
                scenario_path.display()
            ),
        )
        .expect("write project config");

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let prep = runner.prepare(&task).expect("prepare");

        let err = runner
            .run_phase(&prep, &task, attempt, &implement_spec())
            .expect_err("a missing report must never be treated as success");

        assert!(matches!(err, Error::Report { .. }));
        assert!(
            err.to_string()
                .contains(&expected_report_path.display().to_string()),
            "error must name the expected path, got: {err}"
        );

        let worktree = prep.worktree.clone();
        drop(prep);
        crate::remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    /// A [`Provider`] that, once it receives a real (non-empty) prompt,
    /// writes the agent's report at a fixed absolute path, then also writes
    /// and commits a file outside its phase's write scope — proving
    /// `run_phase` catches a scope violation from what was actually
    /// committed ([`crate::changed_paths`]), never from the agent's own
    /// account of what it touched. An empty prompt (`prepare`'s own
    /// availability check) is answered without touching the filesystem,
    /// so this provider is also safe to use for `preflight`, which invokes
    /// it against `project.root` itself rather than an isolated worktree.
    struct ScopeViolator {
        report_path: PathBuf,
    }

    impl Provider for ScopeViolator {
        fn name(&self) -> &'static str {
            "scope-violator"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                structured_output: false,
                model_selection: false,
                usage_telemetry: false,
            }
        }

        fn invoke(&self, inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
            if inv.prompt.is_empty() {
                return Ok(Outcome {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    usage: None,
                    session_id: None,
                });
            }

            std::fs::write(
                &self.report_path,
                "KTASK_RESULT: DONE\nSummary: it worked.\n",
            )?;
            std::fs::write(inv.working_dir.join("forbidden.txt"), "not allowed\n")?;
            crate::git(&inv.working_dir, &["add", "-A"])?;
            crate::git(
                &inv.working_dir,
                &["commit", "--quiet", "-m", "scope violation"],
            )?;

            Ok(Outcome {
                exit_code: 0,
                stdout: "wrote a forbidden file".to_string(),
                stderr: String::new(),
                usage: None,
                session_id: None,
            })
        }
    }

    #[test]
    fn run_phase_fails_as_a_policy_failure_naming_the_forbidden_path_when_a_phase_writes_outside_its_scope()
     {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);
        let expected_report_path = report_path(&project, task.id, attempt);

        let mut runner = manual_runner(
            &project,
            runnable_config(),
            Box::new(ScopeViolator {
                report_path: expected_report_path,
            }),
        );
        let prep = runner.prepare(&task).expect("prepare");

        let err = runner
            .run_phase(&prep, &task, attempt, &verify_spec())
            .expect_err("a write outside the phase's scope must be rejected");

        match err {
            Error::Policy { paths, detail } => {
                assert_eq!(paths, vec![PathBuf::from("forbidden.txt")]);
                assert!(!detail.is_empty());
            }
            other => panic!("expected Error::Policy, got {other:?}"),
        }

        let worktree = prep.worktree.clone();
        drop(prep);
        crate::remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }
}

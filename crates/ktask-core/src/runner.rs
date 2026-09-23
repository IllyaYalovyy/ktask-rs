//! `preflight` and `Runner`: proving the world is sane before spending
//! tokens, then opening the first attempt against it (`VISION.md` §6).
//!
//! The rest of the supervisor loop this module is named for arrives in
//! later tasks; today it holds `preflight` and `Runner`'s construction and
//! `begin_attempt`, the entry points those tasks have needed so far.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::statvfs::statvfs;
use time::OffsetDateTime;

use crate::{
    AttemptId, AttemptRecord, Bounds, Breaker, BreakerState, Bus, Config, Decision, Error,
    EventKind, FailureClass, Gate, GateKind, GateResult, Invocation, Journal, Outcome, PauseReason,
    Phase, PhaseSpec, Profile, Project, Provider, RebaseOutcome, Recorder, RepoLock, ReportResult,
    Request, Result, Stream, Subscription, Task, TaskId, TaskState, TaskStatus, TestSummary,
    WaitPlan, acquire, apply, assemble, build, bundle, changed_paths, check_model,
    check_no_policy_edit, check_scope, claim_tdd_exception, classify, collect_adrs, commit_all,
    create_worktree, ensure_report_dir, fetch, for_task, head_sha, load, load_context_doc,
    load_for, load_template, next_runnable, parse_cargo, parse_reset, profile_from, publish,
    read_evidence, read_report, rebase_onto_remote, redact, remove_worktree, require_clean,
    run_completion_set, run_gate, should_continue, signature, verify_green, verify_red, wait_plan,
    write_evidence,
};

/// How long [`Runner::prepare`] waits for the repository lock ([`acquire`])
/// to become free before giving up. [`preflight`]'s own last check has
/// already confirmed no live holder was found a moment earlier, so this only
/// has to cover the narrow window between that check and this acquisition.
const PREPARE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// The process-wide flag [`interrupt_flag`]'s `SIGINT` handler sets.
static INTERRUPTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Returns the flag this process's `SIGINT` handler sets, installing that
/// handler the first time this is called (`docs/CONTRACT.md` §1: exit code
/// 130, "state is durable and resumable"). Every later call, from a fresh
/// [`Runner`] or from [`super::provider::run_streaming`]'s own output loop,
/// reads the same `Arc` rather than registering a second handler:
/// `signal_hook` itself tolerates more than one flag being registered for
/// the same signal, but nothing here needs more than one.
///
/// Registration is best-effort: the only realistic failure
/// (`signal_hook::low_level::register` rejecting a signal it never allows a
/// handler for, such as `SIGKILL`) does not apply to `SIGINT`, so a flag
/// that silently never gets set is an acceptable, untested-in-practice
/// fallback rather than a reason to make every caller handle an `Err` that
/// does not happen.
pub(crate) fn interrupt_flag() -> Arc<AtomicBool> {
    INTERRUPTED
        .get_or_init(|| {
            let flag = Arc::new(AtomicBool::new(false));
            let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag));
            flag
        })
        .clone()
}

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
    /// Set once this process receives `SIGINT` ([`interrupt_flag`]); checked
    /// at every phase boundary so an interrupt lands as a durable
    /// [`PauseReason::Interrupted`] pause rather than an ordinary failure.
    interrupt: Arc<AtomicBool>,
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
            .field("interrupt", &self.interrupt)
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
        let interrupt = interrupt_flag();

        Ok(Runner {
            project,
            config,
            profile,
            recorder,
            provider,
            interrupt,
        })
    }

    /// Attaches a new subscriber to this runner's bus, so a frontend can
    /// observe every event the runner journals from here on
    /// (`docs/CONTRACT.md` section 0 rule 2: both frontends consume the
    /// same event stream).
    ///
    /// The subscriber sees only events recorded after this call; a
    /// [`Subscription`] is independent of the runner's borrow, so it can be
    /// drained from another thread while [`Runner::run_queue`] runs.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.recorder.subscribe()
    }

    /// True once this process has received `SIGINT` since [`interrupt_flag`]
    /// installed its handler, or an operator has sent
    /// [`Request::Interrupt`] ([`crate::send`]).
    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst) || crate::pending(&self.project, Request::Interrupt)
    }

    /// If an operator asked for `task`'s running attempt to end now — a
    /// [`Request::Cancel`] for `task`, or [`Runner::interrupted`] — journals
    /// what was asked for and returns the resulting durable state;
    /// otherwise `Ok(None)`, so a caller falls through to whatever it would
    /// otherwise have done.
    ///
    /// An interrupt journals [`EventKind::Interrupted`] naming `phase` as
    /// the point to resume from, leaving [`TaskState::Paused`]; a cancel
    /// journals [`EventKind::TaskCancelled`], leaving
    /// [`TaskState::Cancelled`], which the queue proceeds past. A cancel
    /// wins when both are pending. The request is consumed only once the
    /// event is journaled, so a crash in between leaves it to be found (and
    /// discarded by [`Runner::run_queue`]) rather than acted on twice.
    ///
    /// Called at every phase boundary in [`Runner::drive_attempt`] — before a
    /// phase starts, after it returns (whether it succeeded or failed, since
    /// [`super::provider::run_streaming`]'s own output loop may have just
    /// killed the provider's process group in response to the same request)
    /// and after its gate, if any — and again once remediation gives up.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Recorder::record`] returns on failure to journal.
    fn stop_if_requested(&mut self, task: &Task, phase: Phase) -> Result<Option<TaskState>> {
        if crate::pending(&self.project, Request::Cancel(task.id)) {
            self.recorder.record(
                Some(task.id),
                EventKind::TaskCancelled {
                    reason: "cancelled by `ktask-rs cancel`".to_string(),
                },
            )?;
            crate::control::take(&self.project, Request::Cancel(task.id))?;
        } else if self.interrupted() {
            self.recorder
                .record(Some(task.id), EventKind::Interrupted { phase })?;
            crate::control::take(&self.project, Request::Interrupt)?;
        } else {
            return Ok(None);
        }
        Ok(Some(journaled_state(&self.project, task.id)?))
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

        self.open_worktree(task, base_sha)
    }

    /// Takes the repository lock and creates `task`'s isolated worktree at
    /// `base_sha`: what [`Runner::prepare`] and [`Runner::retry_task`] do
    /// once preflight has passed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockTimeout`] if the repository lock cannot be
    /// acquired within a bounded timeout, and whatever [`create_worktree`]
    /// returns if the worktree cannot be created.
    fn open_worktree(&self, task: &Task, base_sha: String) -> Result<Prepared> {
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
        let watch = crate::control::watch(&self.project, task.id);
        let outcome = self.provider.invoke(&inv, None);
        drop(watch);
        let outcome = outcome?;

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

    /// Runs `spec`'s mechanical gate against `prep.worktree` and enforces
    /// the `tdd` protocol's ordering (`VISION.md` §9): the runner confirms
    /// red and green, rather than trusting the agent's word for either.
    ///
    /// A [`Phase::Red`] call must produce a genuinely new test failure
    /// ([`verify_red`]) before it advances. A [`Phase::Green`] call must
    /// turn every test [`Phase::Red`] found newly failing green, without
    /// regressing anything else ([`verify_green`]). Every other gated phase
    /// (`refactor`, the mandatory `verify`) only requires its gate command
    /// to exit successfully.
    ///
    /// `before` is the [`TestSummary`] the previous gated phase produced: the
    /// pre-red baseline for a [`Phase::Red`] call (a missing baseline is
    /// treated as empty — no tests failing yet), and [`Phase::Red`]'s own
    /// returned summary for the [`Phase::Green`] call that follows it, whose
    /// `failures` become [`verify_green`]'s `expected` argument.
    ///
    /// A [`Phase::Red`] call is skipped entirely — no gate runs, no test
    /// comparison is made — when `task` declares a `**TDD-Exception:**`
    /// ([`claim_tdd_exception`]): [`EventKind::TddExceptionUsed`] is recorded
    /// instead, and this returns an empty [`TestSummary`], so the
    /// [`Phase::Green`] call that follows treats "expected" as empty too —
    /// [`verify_green`] then demands the targeted gate be fully green, the
    /// correct bar for a change that was never meant to start red.
    ///
    /// Every gate invocation is bracketed by [`EventKind::GateStarted`] and
    /// [`EventKind::GateFinished`] in `task`'s journal, and its command,
    /// combined output and the worktree's tree hash at the moment it ran are
    /// written to `<state_dir>/attempts/<task>/<attempt>/gates/<phase>.log`
    /// as durable evidence (`VISION.md` §9: "RED and GREEN evidence
    /// (command, output, tree hash) is stored with the attempt").
    ///
    /// # Errors
    ///
    /// Returns [`Error::Policy`] if `spec` names no gate, [`Error::Config`]
    /// if the named gate has no command configured in this project's
    /// profile, whatever [`run_gate`] returns if the command cannot be
    /// spawned, [`Error::Gate`] if the gate's output carries no recognizable
    /// test summary, if a [`Phase::Red`] gate found no new failure
    /// ([`verify_red`]), if a [`Phase::Green`] gate left an expected test
    /// still failing or regressed another ([`verify_green`]), or if any
    /// other gated phase's command exited unsuccessfully; and whatever
    /// [`Recorder::record`], [`head_sha`] or writing the evidence file
    /// return on failure.
    pub fn gate_phase(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        spec: &PhaseSpec,
        before: Option<&TestSummary>,
    ) -> Result<TestSummary> {
        if spec.phase == Phase::Red
            && let Some((exception, reason)) = claim_tdd_exception(task, false)?
        {
            self.recorder.record(
                Some(task.id),
                EventKind::TddExceptionUsed { exception, reason },
            )?;
            return Ok(empty_summary());
        }

        let kind = spec.gate.ok_or_else(|| Error::Policy {
            detail: format!("phase {:?} has no gate configured to run", spec.phase),
            paths: Vec::new(),
        })?;
        let gate = self
            .profile
            .get(kind)
            .cloned()
            .ok_or_else(|| Error::Config {
                key: format!("{kind:?}"),
                detail: "no gate command is configured for this phase".to_string(),
            })?;

        self.recorder
            .record(Some(task.id), EventKind::GateStarted { gate: kind })?;
        let result = run_gate(&gate, &prep.worktree, None)?;
        let tree_hash = head_sha(&prep.worktree)?;
        self.recorder.record(
            Some(task.id),
            EventKind::GateFinished {
                result: result.clone(),
            },
        )?;
        write_gate_evidence(
            &self.project,
            task.id,
            attempt,
            spec.phase,
            &gate,
            &result,
            &tree_hash,
        )?;

        let combined = format!("{}{}", result.stdout, result.stderr);
        let after = parse_cargo(&combined).ok_or_else(|| Error::Gate {
            kind: format!("{kind:?}"),
            detail: "gate output did not contain a recognizable test summary".to_string(),
        })?;

        match spec.phase {
            Phase::Red => {
                let baseline = before.cloned().unwrap_or_else(empty_summary);
                verify_red(&baseline, &after)?;
            }
            Phase::Green => {
                let expected = before.map(|b| b.failures.clone()).unwrap_or_default();
                verify_green(&expected, &after)?;
            }
            _ => {
                if !result.passed {
                    return Err(Error::Gate {
                        kind: format!("{kind:?}"),
                        detail: "gate command failed".to_string(),
                    });
                }
            }
        }

        Ok(after)
    }

    /// Decides whether `task`'s `attempt` is publishable, and publishes it
    /// if so (`VISION.md` §3 invariant 7, §8, §10): a dirty worktree is
    /// rejected outright, the mandatory completion gates
    /// ([`run_completion_set`]) must pass before anything is committed, and
    /// publication itself is only trusted once a fresh fetch confirms the
    /// candidate actually landed on `config.mainline_branch`
    /// ([`crate::publish`]).
    ///
    /// In order:
    ///
    /// 1. [`require_clean`] on `prep.worktree`. A dirty tree is a
    ///    [`FailureClass::PolicyFailure`], journaled as
    ///    [`EventKind::VerifyFailed`] exactly like a failing gate would be —
    ///    `VISION.md` §10 names this explicitly ("a dirty tree at
    ///    verification time is a `policy_failure`").
    /// 2. [`run_completion_set`] against `prep.base_sha`, recording
    ///    [`EventKind::VerifyPassed`] if every configured gate passed, or
    ///    [`EventKind::VerifyFailed`] otherwise.
    /// 3. [`commit_all`], capturing anything the completion gates
    ///    themselves changed (a mutating `format_command`, for instance) —
    ///    [`Error::NothingToCommit`] is not an error here, since a
    ///    well-behaved provider already committed its own work during
    ///    [`Runner::run_phase`]; the candidate is simply `prep.worktree`'s
    ///    current `HEAD` in that case.
    /// 4. Records [`EventKind::PublishStarted`] naming the candidate SHA,
    ///    then calls [`crate::publish`].
    ///
    /// A push [`crate::publish`] rejects is recovered mechanically: fetch
    /// and replay onto the new remote tip ([`rebase_onto_remote`]). A clean
    /// divergence reruns the completion set from scratch — nothing survives
    /// a rebase's file changes uninspected — and retries publication exactly
    /// once more; a conflicting divergence is returned as
    /// [`Error::Git`] rather than attempted automatically, since resolving a
    /// real conflict is a human's call, not an agent's (`VISION.md` §7 lists
    /// "conflicting publication" itself as `git_conflict`).
    ///
    /// No further [`EventKind::VerifyFailed`] or [`EventKind::PublishStarted`]
    /// is recorded on the retried path: `task`'s pipeline state has already
    /// moved to `Publishing` by the first [`EventKind::PublishStarted`], and
    /// that state only accepts [`EventKind::PublishVerified`] as its next
    /// success event (`state.rs`'s `from_publishing`) — the retry's own
    /// completion-set failure or renewed rejection is reported purely
    /// through this call's `Err`.
    ///
    /// Returns the published commit's SHA on success.
    ///
    /// # Errors
    ///
    /// Returns whatever [`require_clean`], [`run_completion_set`],
    /// [`commit_all`] (other than [`Error::NothingToCommit`]),
    /// [`crate::publish`] or [`rebase_onto_remote`] themselves return;
    /// [`Error::Gate`] if the completion set (first run or retry) left any
    /// gate failing; [`Error::Git`] if a rebase onto the fetched remote tip
    /// conflicts; and whatever [`Recorder::record`] returns on failure to
    /// journal.
    pub fn verify_and_publish(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
    ) -> Result<String> {
        if let Err(err) = require_clean(&prep.worktree) {
            let class = classify(&empty_outcome(), &[], Some(&err));
            self.recorder.record(
                Some(task.id),
                EventKind::VerifyFailed {
                    attempt,
                    class,
                    detail: err.to_string(),
                },
            )?;
            return Err(err);
        }

        self.run_completion_gates(prep, task, attempt)?;

        let candidate_sha = match commit_all(&prep.worktree, &commit_message(task)) {
            Ok(sha) => sha,
            Err(Error::NothingToCommit { .. }) => head_sha(&prep.worktree)?,
            Err(err) => return Err(err),
        };

        self.recorder.record(
            Some(task.id),
            EventKind::PublishStarted {
                attempt,
                candidate_sha: candidate_sha.clone(),
            },
        )?;

        match publish(
            &prep.worktree,
            &self.config.mainline_remote,
            &self.config.mainline_branch,
            &candidate_sha,
        ) {
            Ok(()) => self.record_published(task, &candidate_sha),
            Err(err) if is_rejected_push(&err) => self.retry_after_rebase(prep, task),
            Err(err) => Err(err),
        }
    }

    /// Drains `tasks` in order, driving each runnable one through
    /// [`Runner::run_task`] (`VISION.md` §3 invariant 2: a successor never
    /// starts before its predecessor settles) until nothing is left to run
    /// or something needs a human: a failed attempt or a durable pause.
    ///
    /// `from`, when given, restricts `tasks` to those with an id at or after
    /// it before selection ever runs (`docs/CONTRACT.md`'s `run --from`): an
    /// excluded task's state plays no part in [`next_runnable`]'s
    /// predecessor check, so the drained subset starts exactly at `from`
    /// rather than being blocked behind whatever an excluded predecessor's
    /// state happens to be.
    ///
    /// Before every selection, the candidates' current [`TaskState`] is
    /// rebuilt fresh from each one's own journal ([`Journal::events_for`]) —
    /// a task with no recorded event yet defaults to [`TaskState::Queued`],
    /// matching [`next_runnable`]'s own contract. [`next_runnable`]
    /// returning `Ok(None)` means every candidate has reached a settled
    /// state: the run is [`RunOutcome::Drained`].
    ///
    /// Once a task is selected, [`Runner::run_task`] drives it to
    /// completion. An `Err` from that call stops the run at
    /// [`RunOutcome::TaskFailed`] without ever selecting again. A durable
    /// [`TaskState::Paused`] it returns instead is translated to the
    /// matching [`RunOutcome`]: [`crate::PauseReason::HumanGate`] to
    /// [`RunOutcome::HumanGate`], [`crate::PauseReason::Input`] to
    /// [`RunOutcome::NeedsInput`], [`crate::PauseReason::Limit`] to
    /// [`RunOutcome::ProviderLimit`]. [`crate::PauseReason::Interrupted`] (an
    /// interrupt, from `SIGINT` or an operator's [`Request::Interrupt`]) is a
    /// pause rather than a failure, so it is reported as a resumable
    /// [`RunOutcome::Interrupted`], as is [`crate::PauseReason::Blocked`].
    /// Every other state `run_task` returns, `Done` and an operator's
    /// [`Request::Cancel`] (which leaves [`TaskState::Cancelled`]) chief among
    /// them, is settled progress, and selection continues.
    ///
    /// Operator requests ([`crate::send`]) are honored as follows.
    /// Whatever was pending when the run started is discarded: it was meant
    /// for a supervisor that has since exited. A task left
    /// [`crate::PauseReason::Blocked`] by an earlier [`Request::Pause`] is
    /// resumed ([`EventKind::Resumed`]), which is how running the queue
    /// again lifts the pause. A [`Request::Pause`] is acted on between tasks:
    /// the task in flight finishes, and the next one is parked as
    /// [`crate::PauseReason::Blocked`] instead of started, so the queue
    /// stops with somewhere durable to resume from. A pause sent during the
    /// last task finds nothing left to park, and the queue simply drains.
    /// [`Request::Interrupt`] and [`Request::Cancel`] end the task in flight
    /// (`Runner::stop_if_requested`); an interrupt that lands between
    /// tasks stops the queue before the next one starts.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Journal::open_for`], [`Journal::events_for`] or
    /// [`next_runnable`] themselves return on failure to read a candidate's
    /// journaled state, and [`Error::Policy`] in the defensive case where
    /// [`next_runnable`] selects an id [`next_runnable`]'s own contract
    /// guarantees never happens: one absent from `candidates`.
    pub fn run_queue(&mut self, tasks: &[Task], from: Option<TaskId>) -> Result<RunOutcome> {
        let candidates: Vec<Task> = tasks
            .iter()
            .filter(|task| from.is_none_or(|from| task.id >= from))
            .cloned()
            .collect();

        // Requests are for the supervisor that was running when they were
        // sent; any still here belong to one that has since exited.
        crate::control::clear(&self.project)?;
        // Running the queue again is how an operator's pause is lifted: a
        // task parked as `Blocked` is put back where it was.
        for task in &candidates {
            if matches!(
                journaled_state(&self.project, task.id)?,
                TaskState::Paused {
                    reason: PauseReason::Blocked,
                    ..
                }
            ) {
                self.recorder.record(Some(task.id), EventKind::Resumed)?;
            }
        }

        loop {
            let mut states = BTreeMap::new();
            for task in &candidates {
                states.insert(task.id, journaled_state(&self.project, task.id)?);
            }

            let Some(next_id) = next_runnable(&candidates, &states)? else {
                return Ok(RunOutcome::Drained);
            };
            let Some(task) = candidates.iter().find(|task| task.id == next_id) else {
                return Err(Error::Policy {
                    detail: format!(
                        "next_runnable selected task {next_id}, which is not among the candidates"
                    ),
                    paths: Vec::new(),
                });
            };

            // Between tasks, not just between phases within one: a signal
            // caught while nothing was running yet still stops the queue
            // rather than starting one more task first.
            if self.interrupted() {
                crate::control::take(&self.project, Request::Interrupt)?;
                return Ok(RunOutcome::Interrupted);
            }
            // The running task has finished; this is the safe boundary an
            // operator's pause waits for. The pause parks the task that
            // would have run next, so the queue has somewhere durable to
            // resume from.
            if crate::control::take(&self.project, Request::Pause)? {
                self.recorder.record(
                    Some(task.id),
                    EventKind::Paused {
                        reason: PauseReason::Blocked,
                    },
                )?;
                return Ok(RunOutcome::Interrupted);
            }

            let result = self.run_task(task);
            // A cancel that missed its task's last boundary must not go on
            // to cancel a task that has already finished.
            crate::control::take(&self.project, Request::Cancel(task.id))?;
            match result {
                Ok(TaskState::Paused { reason, .. }) => {
                    return Ok(match reason {
                        PauseReason::HumanGate => RunOutcome::HumanGate { task: next_id },
                        PauseReason::Input => RunOutcome::NeedsInput { task: next_id },
                        PauseReason::Limit { until } => RunOutcome::ProviderLimit { until },
                        PauseReason::Interrupted | PauseReason::Blocked => RunOutcome::Interrupted,
                    });
                }
                Ok(_) => {}
                Err(_) => return Ok(RunOutcome::TaskFailed { task: next_id }),
            }
        }
    }

    /// Drives `task` through its chosen protocol end to end (`VISION.md`
    /// §6): [`Runner::prepare`] proves the world is sane and opens the
    /// isolated worktree, then this runner's private inner loop runs every
    /// agent-driven phase, the mandatory completion gates, and records
    /// [`EventKind::TaskDone`] once publication is verified.
    ///
    /// A `task` whose status is [`TaskStatus::HumanGate`] never reaches any
    /// of that: `VISION.md` §6's gate "is never handed to an agent" and
    /// "produces no commit", so this records [`EventKind::Paused`] with
    /// [`crate::PauseReason::HumanGate`] straight from [`TaskState::Queued`]
    /// and returns — no worktree is created, no lock is acquired, and
    /// [`Runner::prepare`] is never called.
    ///
    /// The worktree [`Runner::prepare`] created is always removed
    /// ([`remove_worktree`]) and the repository lock it held is always
    /// released, on every exit path — whether `task` reaches
    /// [`TaskState::Done`], a durable pause, or a step fails partway. A
    /// failure removing the worktree is only surfaced when the attempt
    /// itself otherwise succeeded; an attempt's own failure is never masked
    /// by a subsequent cleanup failure.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Runner::prepare`], [`Runner::run_phase`],
    /// [`Runner::gate_phase`], [`Runner::verify_and_publish`] or
    /// [`remove_worktree`] themselves return.
    pub fn run_task(&mut self, task: &Task) -> Result<TaskState> {
        if task.status == TaskStatus::HumanGate {
            self.recorder.record(
                Some(task.id),
                EventKind::Paused {
                    reason: PauseReason::HumanGate,
                },
            )?;
            return journaled_state(&self.project, task.id);
        }

        let prep = self.prepare(task)?;
        let worktree = prep.worktree.clone();

        let outcome = self
            .drive_attempt(&prep, task)
            .or_else(|err| self.journal_failure(task, err));
        drop(prep);
        let removed = remove_worktree(&self.project.root, &worktree);

        outcome.and_then(|state| removed.map(|()| state))
    }

    /// Journals `err`, which ended `task`'s attempt, as the task's
    /// [`EventKind::TaskFailed`] so it is `Failed` — and `retry`able — rather
    /// than stranded mid-attempt, then returns `err` unchanged.
    ///
    /// A task that is not mid-attempt is left alone: a pause or interrupt
    /// already journaled its own verdict, and a failure with nothing in
    /// flight (the journal could not be read, say) has nothing to close.
    ///
    /// # Errors
    ///
    /// Always returns `err`, or whatever [`Runner::record_failure`] or
    /// [`read_evidence`] returns if the failure cannot be journaled.
    fn journal_failure(&mut self, task: &Task, err: Error) -> Result<TaskState> {
        let in_flight = matches!(
            journaled_state(&self.project, task.id),
            Ok(TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
                | TaskState::Publishing { .. })
        );
        if in_flight {
            let attempts = read_evidence(&self.project, task.id)?.len();
            let attempt = AttemptId::new(u32::try_from(attempts).unwrap_or(u32::MAX).max(1));
            let class = classify(
                &empty_outcome(),
                &gates_for_classification(&err),
                Some(&err),
            );
            self.record_failure(task, attempt, class, err.to_string())?;
        }
        Err(err)
    }

    /// Starts a fresh remediation attempt for a [`TaskState::Failed`]
    /// `task` and drives it to a verdict (`ktask-rs retry`, `VISION.md` §7).
    ///
    /// The attempt runs in a new provider session — never a resumed one —
    /// whose prompt is the same compact failure bundle a remediation round
    /// gets ([`bundle`]): the recorded failure's classification and detail,
    /// the gate output the failed attempt journaled, a summary of the fresh
    /// worktree's diff, and every prior attempt's evidence. The failed
    /// attempt's own worktree is gone (every run removes its worktree), so
    /// nothing but that evidence carries over.
    ///
    /// Preflight runs first, against the world as it is now, since a retry
    /// is usually what follows the human fixing something: if it fails, the
    /// task stays [`TaskState::Failed`] and nothing is recorded against it.
    /// Once it passes, [`EventKind::RetryStarted`] is journaled — the only
    /// event that leaves `Failed` — naming the new attempt, one past every
    /// attempt already evidenced. Then, exactly as in a remediation round,
    /// the agent runs (`run_remediation_phase`), the mandatory
    /// completion gates rerun from scratch and the result is published
    /// ([`Runner::verify_and_publish`]).
    ///
    /// A retry that succeeds records [`EventKind::TaskDone`]. One that fails
    /// records [`EventKind::TaskFailed`] again, so the task is `Failed` (and
    /// retryable) rather than stranded mid-attempt, and returns the error
    /// that failed it. The worktree is removed and the lock released on
    /// every path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTransition`] if `task` is not
    /// [`TaskState::Failed`], [`Error::Preflight`] if the world is still not
    /// sane, and whatever creating the worktree returns; otherwise the
    /// error the retried attempt failed with.
    pub fn retry_task(&mut self, task: &Task) -> Result<TaskState> {
        let TaskState::Failed { class, detail } = journaled_state(&self.project, task.id)? else {
            let state = journaled_state(&self.project, task.id)?;
            return Err(Error::InvalidTransition {
                from: state.name().to_string(),
                event: "RetryStarted".to_string(),
            });
        };

        let base_sha = match preflight(&self.project, &self.config, self.provider.as_ref())? {
            PreflightReport::Passed { base_sha } => base_sha,
            PreflightReport::Failed { class, detail } => {
                return Err(Error::Preflight { class, detail });
            }
        };
        let prep = self.open_worktree(task, base_sha)?;
        let worktree = prep.worktree.clone();

        let outcome = self.drive_retry(&prep, task, class, &detail);
        drop(prep);
        let removed = remove_worktree(&self.project.root, &worktree);

        outcome.and_then(|state| removed.map(|()| state))
    }

    /// [`Runner::retry_task`]'s inner loop, run against `prep`'s already-open
    /// worktree and lock, for a task that failed with `class` and `detail`.
    fn drive_retry(
        &mut self,
        prep: &Prepared,
        task: &Task,
        class: FailureClass,
        detail: &str,
    ) -> Result<TaskState> {
        let attempt = self.next_evidence_attempt_id(task.id)?;
        let gates = self.failed_attempt_gates(task.id, detail)?;
        let prior = read_evidence(&self.project, task.id)?;
        let budget = usize::try_from(self.config.failure_bundle_bytes).unwrap_or(usize::MAX);
        let text = bundle(task, class, &gates, &diff_summary(prep)?, &prior, budget);

        self.recorder
            .record(Some(task.id), EventKind::RetryStarted { attempt })?;

        let round = self
            .run_remediation_phase(prep, task, attempt, &text)
            .and_then(|_outcome| {
                self.recorder.record(
                    Some(task.id),
                    EventKind::PhaseEntered {
                        attempt,
                        phase: Phase::Verify,
                    },
                )?;
                self.verify_and_publish(prep, task, attempt)
            });

        match round {
            Ok(commit) => {
                self.write_remediation_evidence(
                    prep,
                    task,
                    attempt,
                    "remediated".to_string(),
                    Some(commit.clone()),
                )?;
                self.recorder
                    .record(Some(task.id), EventKind::TaskDone { commit })?;
                journaled_state(&self.project, task.id)
            }
            Err(err) => {
                if let Some(state) = self.stop_if_requested(task, Phase::Implement)? {
                    return Ok(state);
                }
                let class = classify(
                    &empty_outcome(),
                    &gates_for_classification(&err),
                    Some(&err),
                );
                self.write_remediation_evidence(
                    prep,
                    task,
                    attempt,
                    format!("{class:?}: {err}"),
                    None,
                )?;
                self.record_failure(task, attempt, class, err.to_string())?;
                Err(err)
            }
        }
    }

    /// Journals `task`'s failure as [`EventKind::TaskFailed`], first
    /// recording the [`EventKind::VerifyFailed`] `state.rs` requires to leave
    /// [`TaskState::Verifying`] when the attempt failed there without having
    /// recorded one (a commit that could not be made, say).
    fn record_failure(
        &mut self,
        task: &Task,
        attempt: AttemptId,
        class: FailureClass,
        detail: String,
    ) -> Result<()> {
        if matches!(
            journaled_state(&self.project, task.id)?,
            TaskState::Verifying { .. }
        ) {
            self.recorder.record(
                Some(task.id),
                EventKind::VerifyFailed {
                    attempt,
                    class,
                    detail: detail.clone(),
                },
            )?;
        }
        self.recorder
            .record(Some(task.id), EventKind::TaskFailed { class, detail })?;
        Ok(())
    }

    /// The failing gates the journal holds for `task`'s most recent attempt
    /// (everything since its last [`EventKind::AttemptStarted`] or
    /// [`EventKind::RetryStarted`]), for a failure bundle.
    ///
    /// A failure that ran no gate — preflight, an agent that never
    /// reported — has none, so the recorded `detail` stands in as one
    /// failing gate's output: the same stand-in
    /// [`gates_for_classification`] builds, and for the same reason, that
    /// the failure text reaches the bundle's "Failing gates" section.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Journal::open_for`] or [`Journal::events_for`]
    /// return on failure to read the journal.
    fn failed_attempt_gates(&self, task: TaskId, detail: &str) -> Result<Vec<GateResult>> {
        let journal = Journal::open_for(&self.project)?;
        let mut gates = Vec::new();
        for event in journal.events_for(task)? {
            match event.kind {
                EventKind::AttemptStarted { .. } | EventKind::RetryStarted { .. } => {
                    gates.clear();
                }
                EventKind::GateFinished { result } if !result.passed => gates.push(result),
                _ => {}
            }
        }
        if gates.is_empty() {
            gates.push(GateResult {
                kind: GateKind::Verify,
                passed: false,
                exit_code: None,
                signal: None,
                duration_ms: 0,
                stdout: String::new(),
                stderr: detail.to_string(),
                timed_out: false,
            });
        }
        Ok(gates)
    }

    /// [`Runner::run_task`]'s inner loop, run against `prep`'s already-open
    /// worktree and lock: opens the attempt ([`Runner::begin_attempt`]),
    /// runs [`Runner::run_phase`] then, where the phase names a gate,
    /// [`Runner::gate_phase`] for every phase of `task`'s protocol except
    /// its mandatory [`Phase::Verify`]/[`Phase::Publish`] tail (every
    /// [`crate::Protocol`] ends with exactly that pair,
    /// `protocol.rs`'s `checked` guarantees it) — those two are entirely
    /// mechanical and are instead driven by [`Runner::verify_and_publish`].
    ///
    /// Immediately before calling it, records
    /// [`EventKind::PhaseEntered`] naming [`Phase::Verify`] itself: this is
    /// the one transition `state.rs`'s `from_running` recognizes as moving
    /// a task from `Running`/`Remediating` into [`TaskState::Verifying`],
    /// and neither [`Runner::verify_and_publish`] nor anything it calls
    /// emits it, so nothing else in this call graph would.
    ///
    /// Returns the [`TaskState`] [`crate::apply`] derives from replaying
    /// every event this attempt journaled for `task`, from
    /// [`TaskState::Queued`] — never a value assumed because control flow
    /// reached the end without an `Err` (`VISION.md` §3 invariant 4).
    ///
    /// Two of `VISION.md` §6's durable pauses are decided here rather than
    /// ever reaching [`Runner::remediate`] as an ordinary failure: a report
    /// claiming [`ReportResult::NeedsInput`] records
    /// [`EventKind::DecisionRaised`] and returns the resulting
    /// [`TaskState::Paused`] with [`crate::PauseReason::Input`] immediately
    /// (§7: "`needs_input` never loop; they pause for the human
    /// immediately"); a phase whose provider reported a usage limit is
    /// recognized by [`Runner::pause_for_provider_limit`] and turned into
    /// [`TaskState::Paused`] with [`crate::PauseReason::Limit`] the same
    /// way. Neither ever reaches [`EventKind::TaskFailed`].
    ///
    /// A third pause is decided the same way, at every phase boundary
    /// rather than only between attempts: [`Runner::stop_if_requested`]
    /// checks whether this process has caught `SIGINT`, or an operator has
    /// sent an interrupt or a cancel for this task — before a phase begins,
    /// after it ends (success or failure alike, since
    /// [`super::provider::run_streaming`]'s own output loop may have just
    /// killed the provider's process group in response to the same
    /// request), and after its gate. Whichever check first finds one
    /// records [`EventKind::Interrupted`] naming the phase to resume from
    /// (or [`EventKind::TaskCancelled`]) and returns immediately, before
    /// the loop or [`Runner::remediate`] ever get a chance to treat it as
    /// an ordinary failure. The same check runs once [`Runner::remediate`]
    /// gives up, for a request that arrived while it was working.
    fn drive_attempt(&mut self, prep: &Prepared, task: &Task) -> Result<TaskState> {
        let attempt = self.begin_attempt(task)?;
        let protocol = for_task(task, &self.config)?;
        let split = protocol.phases.len().saturating_sub(2);
        let (agent_phases, _verify_and_publish) = protocol.phases.split_at(split);

        let mut before: Option<TestSummary> = None;
        for spec in agent_phases {
            if let Some(state) = self.stop_if_requested(task, spec.phase)? {
                return Ok(state);
            }
            let phase_outcome = match self.run_phase(prep, task, attempt, spec) {
                Ok(phase_outcome) => phase_outcome,
                Err(err) => {
                    if let Some(state) = self.stop_if_requested(task, spec.phase)? {
                        return Ok(state);
                    }
                    return match self.pause_for_provider_limit(task, attempt, &err)? {
                        Some(state) => Ok(state),
                        None => Err(err),
                    };
                }
            };
            if let Some(state) = self.stop_if_requested(task, spec.phase)? {
                return Ok(state);
            }
            if let ReportResult::NeedsInput(request) = phase_outcome.report {
                self.recorder
                    .record(Some(task.id), EventKind::DecisionRaised { request })?;
                return journaled_state(&self.project, task.id);
            }
            if spec.gate.is_some() {
                before = Some(self.gate_phase(prep, task, attempt, spec, before.as_ref())?);
                if let Some(state) = self.stop_if_requested(task, spec.phase)? {
                    return Ok(state);
                }
            }
        }

        self.recorder.record(
            Some(task.id),
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )?;

        let commit = match self.verify_and_publish(prep, task, attempt) {
            Ok(commit) => commit,
            Err(err) => match self.remediate(prep, task, attempt, err) {
                Ok(commit) => commit,
                Err(err) => {
                    // A round cut short by an interrupt or a cancel fails
                    // like any other; the request is what it was, not the
                    // failure it looks like.
                    return match self.stop_if_requested(task, Phase::Implement)? {
                        Some(state) => Ok(state),
                        None => Err(err),
                    };
                }
            },
        };
        self.recorder
            .record(Some(task.id), EventKind::TaskDone { commit })?;

        journaled_state(&self.project, task.id)
    }

    /// After [`Runner::run_phase`] returns `err` for `task`'s `attempt`,
    /// decides whether the provider actually hit a usage limit rather than
    /// merely never writing the report `err` names as missing, and if so
    /// pauses `task` instead of letting `err` propagate as an ordinary
    /// failure (`VISION.md` §7: `provider_limit` is never `agent_failure`).
    ///
    /// The limit text is recovered from [`EventKind::AgentOutput`] —
    /// [`Runner::run_phase`] journals it before it ever tries to read the
    /// report, so a provider that stopped mid-run because of a limit still
    /// leaves it behind even though it wrote no report at all. Reclassifying
    /// that recovered text with [`classify()`] is what tells a genuine
    /// limit apart from every other reason a report could be missing.
    ///
    /// A recognized reset ([`parse_reset`]) becomes [`wait_plan`]'s deadline,
    /// plus `config.limit_wait_margin_secs`; an unrecognized or already-past
    /// one leaves `until` unknown (`VISION.md` §7: "unknown resets use
    /// bounded backoff") rather than guessing one. Either way this never
    /// blocks waiting the limit out — it records [`EventKind::Paused`] with
    /// [`crate::PauseReason::Limit`] and returns immediately, leaving `task`
    /// resumable once the limit is believed to have lifted.
    ///
    /// Returns `Ok(None)` — doing nothing — when `err` is not
    /// [`Error::Report`], or when the recovered text does not classify as
    /// [`FailureClass::ProviderLimit`], so the caller propagates `err`
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Journal::open_for`] or [`Journal::events_for`]
    /// return on failure to read `attempt`'s journaled output back, and
    /// whatever [`Recorder::record`] returns on failure to journal the pause.
    fn pause_for_provider_limit(
        &mut self,
        task: &Task,
        attempt: AttemptId,
        err: &Error,
    ) -> Result<Option<TaskState>> {
        if !matches!(err, Error::Report { .. }) {
            return Ok(None);
        }

        let (stdout, stderr) = self.last_agent_output(task.id, attempt)?;
        let outcome = Outcome {
            exit_code: 1,
            stdout,
            stderr,
            usage: None,
            session_id: None,
        };
        if classify(&outcome, &[], None) != FailureClass::ProviderLimit {
            return Ok(None);
        }

        let now = OffsetDateTime::now_utc();
        let text = format!("{}\n{}", outcome.stdout, outcome.stderr);
        let reset = parse_reset(&text, now);
        let margin = time::Duration::seconds(
            i64::try_from(self.config.limit_wait_margin_secs).unwrap_or(i64::MAX),
        );
        let max = time::Duration::seconds(
            i64::try_from(self.config.limit_max_wait_secs).unwrap_or(i64::MAX),
        );
        let until = match wait_plan(reset, now, margin, max) {
            WaitPlan::Deadline(at) => Some(at),
            WaitPlan::Backoff(_) => None,
        };

        self.recorder.record(
            Some(task.id),
            EventKind::Paused {
                reason: PauseReason::Limit { until },
            },
        )?;
        Ok(Some(journaled_state(&self.project, task.id)?))
    }

    /// The stdout and stderr [`EventKind::AgentOutput`] already journaled
    /// for `task`'s `attempt`, read back from the journal rather than
    /// threaded through as a parameter: by the time a caller needs it,
    /// [`Runner::run_phase`] has already recorded it as durable evidence, so
    /// there is nothing gained by carrying the live value any further than
    /// that call's own body needs it.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Journal::open_for`] or [`Journal::events_for`]
    /// return on failure to open or read the journal.
    fn last_agent_output(&self, task: TaskId, attempt: AttemptId) -> Result<(String, String)> {
        let journal = Journal::open_for(&self.project)?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        for event in journal.events_for(task)? {
            if let EventKind::AgentOutput {
                attempt: recorded,
                stream,
                text,
            } = event.kind
                && recorded == attempt
            {
                match stream {
                    Stream::Stdout => stdout = text,
                    Stream::Stderr => stderr = text,
                }
            }
        }
        Ok((stdout, stderr))
    }

    /// Bounded, mechanical recovery from a failed attempt (`VISION.md` §7):
    /// classifies why `failed_attempt` failed, seeds a fresh provider
    /// session with a compact failure bundle built from every attempt's
    /// evidence so far, and reruns the mandatory completion gates from
    /// scratch against whatever that session produced.
    ///
    /// Loops until either a retry publishes successfully, [`Breaker::record`]
    /// reports [`BreakerState::Tripped`] on a repeated failure signature, or
    /// [`should_continue`] reports [`Decision::Stop`] against
    /// `config.max_remediation_attempts` and `config.attempt_timeout_secs`
    /// (`VISION.md` §7: "Bound remediation by attempts, elapsed time, and
    /// token budget"). No token budget is enforced: nothing in `Config` names
    /// one, and [`Bounds::max_tokens`] is `None` in exactly that case.
    ///
    /// Each round gets its own [`AttemptId`], one past every attempt already
    /// evidenced for `task` ([`read_evidence`]) — never `begin_attempt`,
    /// which would journal a second [`EventKind::AttemptStarted`] that
    /// `state.rs` only ever accepts from [`TaskState::Preflight`]. A round's
    /// provider invocation never carries the failed attempt's session
    /// forward: [`Invocation`] has no field for one, and the retried
    /// attempt's own evidence records only the session its own [`Outcome`]
    /// reports.
    ///
    /// Every round's evidence is written before this returns — a failed
    /// round's exit reason and classification, or a successful round's
    /// candidate commit — so [`read_evidence`] always reflects exactly how
    /// many attempts `task` actually took, including remediation.
    ///
    /// # Errors
    ///
    /// Returns the triggering error, or the last round's, once the circuit
    /// breaker trips or the bounds are exhausted; whatever
    /// [`Runner::run_remediation_phase`] or [`Runner::verify_and_publish`]
    /// themselves return from a round that fails for a new reason; and
    /// whatever [`write_evidence`] or [`read_evidence`] return on failure to
    /// persist or read evidence.
    fn remediate(
        &mut self,
        prep: &Prepared,
        task: &Task,
        failed_attempt: AttemptId,
        mut err: Error,
    ) -> Result<String> {
        let bounds = Bounds {
            max_attempts: self.config.max_remediation_attempts,
            max_elapsed: Duration::from_secs(self.config.attempt_timeout_secs),
            max_tokens: None,
        };
        let mut breaker = Breaker::new(self.config.circuit_breaker_threshold);
        let clock = Instant::now();
        let mut rounds: u32 = 0;
        let mut last_attempt = failed_attempt;

        loop {
            let gates = gates_for_classification(&err);
            let class = classify(&empty_outcome(), &gates, Some(&err));
            self.write_remediation_evidence(
                prep,
                task,
                last_attempt,
                format!("{class:?}: {err}"),
                None,
            )?;

            if let BreakerState::Tripped { .. } = breaker.record(&signature(class, &gates)) {
                return Err(err);
            }
            if let Decision::Stop(_) = should_continue(&bounds, rounds, clock.elapsed(), 0) {
                return Err(err);
            }
            // No new round for a request that is waiting: the caller finds
            // it and records it in place of the failure `err` describes.
            if self.interrupted() || crate::pending(&self.project, Request::Cancel(task.id)) {
                return Err(err);
            }
            rounds += 1;

            let prior = read_evidence(&self.project, task.id)?;
            let diff_summary = diff_summary(prep)?;
            let budget = usize::try_from(self.config.failure_bundle_bytes).unwrap_or(usize::MAX);
            let text = bundle(task, class, &gates, &diff_summary, &prior, budget);
            let retry_attempt = self.next_evidence_attempt_id(task.id)?;

            let round = self
                .run_remediation_phase(prep, task, retry_attempt, &text)
                .and_then(|_outcome| {
                    self.recorder.record(
                        Some(task.id),
                        EventKind::PhaseEntered {
                            attempt: retry_attempt,
                            phase: Phase::Verify,
                        },
                    )?;
                    self.verify_and_publish(prep, task, retry_attempt)
                });

            match round {
                Ok(commit) => {
                    self.write_remediation_evidence(
                        prep,
                        task,
                        retry_attempt,
                        "remediated".to_string(),
                        Some(commit.clone()),
                    )?;
                    return Ok(commit);
                }
                Err(next_err) => {
                    err = next_err;
                    last_attempt = retry_attempt;
                }
            }
        }
    }

    /// One remediation round's provider invocation (`VISION.md` §7): a
    /// fresh session prompted with `bundle_text` alone rather than
    /// [`crate::assemble`]'s usual context, since the failure bundle already
    /// carries everything a retry needs — classification, failing gate
    /// output, the diff summary, and prior attempt evidence.
    ///
    /// Mirrors [`Runner::run_phase`]'s [`Phase::Implement`] handling
    /// (`PhaseEntered`, invoke, `check_model`, `AgentOutput`,
    /// `AttemptFinished`, [`crate::read_report`]) but checks the resulting
    /// diff with [`check_no_policy_edit`] rather than [`check_scope`]:
    /// `VISION.md` §7's "self-healing ... never edits gate definitions"
    /// applies regardless of the phase's own write scope.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Recorder::record`] returns on failure to journal;
    /// whatever `self.provider.invoke` returns if it cannot be invoked;
    /// [`Error::Provider`] if the configured and reported models mismatch;
    /// [`Error::Report`] if the agent never wrote the report it was told to;
    /// and [`Error::Policy`] naming every path [`check_no_policy_edit`]
    /// rejected.
    fn run_remediation_phase(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        bundle_text: &str,
    ) -> Result<PhaseOutcome> {
        self.recorder.record(
            Some(task.id),
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Implement,
            },
        )?;

        let mut prompt = bundle_text.to_string();
        prompt.push('\n');
        prompt.push_str(&self.name_report_path(task, attempt)?);

        let inv = Invocation {
            prompt,
            model: self.config.model.clone(),
            working_dir: prep.worktree.clone(),
        };
        let watch = crate::control::watch(&self.project, task.id);
        let outcome = self.provider.invoke(&inv, None);
        drop(watch);
        let outcome = outcome?;

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
        check_no_policy_edit(&changed)?;

        Ok(PhaseOutcome { report, changed })
    }

    /// The next [`AttemptId`] to evidence for `task`: one past every attempt
    /// [`read_evidence`] already finds on disk, independent of
    /// [`EventKind::AttemptStarted`] ([`next_attempt_id`] counts those
    /// instead, and only [`Runner::begin_attempt`] should ever call that).
    ///
    /// # Errors
    ///
    /// Returns whatever [`read_evidence`] returns on failure to read.
    fn next_evidence_attempt_id(&self, task_id: TaskId) -> Result<AttemptId> {
        let count = read_evidence(&self.project, task_id)?.len();
        Ok(AttemptId::new(u32::try_from(count).unwrap_or(u32::MAX) + 1))
    }

    /// Writes (or overwrites) `attempt`'s evidence with a concluded
    /// [`AttemptRecord`]: `exit_reason` describes how the round ended, and
    /// `candidate_sha` is `Some` only for a round that published
    /// successfully. Never carries a session id: `VISION.md` §7's remediation
    /// sessions are never resumed, so nothing here has one to preserve.
    ///
    /// # Errors
    ///
    /// Returns whatever [`write_evidence`] returns on failure to persist.
    fn write_remediation_evidence(
        &self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        exit_reason: String,
        candidate_sha: Option<String>,
    ) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: attempt,
            task: task.id,
            started: now,
            ended: Some(now),
            model_configured: self.config.model.clone(),
            model_reported: None,
            session_id: None,
            exit_reason,
            gates: Vec::new(),
            usage: None,
            base_sha: prep.base_sha.clone(),
            candidate_sha,
        };
        write_evidence(&self.project, &record, "")
    }

    /// The rejected-push recovery path of [`Runner::verify_and_publish`]:
    /// fetches and replays `prep.worktree` onto the freshly fetched tip of
    /// `config.mainline_branch`, reruns the completion set from scratch
    /// against the rebased tree, and retries publication exactly once.
    fn retry_after_rebase(&mut self, prep: &Prepared, task: &Task) -> Result<String> {
        match rebase_onto_remote(
            &prep.worktree,
            &self.config.mainline_remote,
            &self.config.mainline_branch,
        )? {
            RebaseOutcome::Applied { new_sha } => {
                self.check_completion_gates(prep)?;
                publish(
                    &prep.worktree,
                    &self.config.mainline_remote,
                    &self.config.mainline_branch,
                    &new_sha,
                )?;
                self.record_published(task, &new_sha)
            }
            RebaseOutcome::Conflict { paths } => Err(Error::Git {
                args: vec!["rebase".to_string(), "conflict".to_string()],
                stderr: format!(
                    "rebase onto {}/{} conflicted on: {}",
                    self.config.mainline_remote,
                    self.config.mainline_branch,
                    paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }
    }

    /// Records [`EventKind::PublishVerified`] for `commit`, once
    /// [`crate::publish`] has itself already confirmed `commit` is the
    /// freshly fetched tip of `config.mainline_branch` — so `remote_sha`
    /// always equals `commit` here, never a stale or divergent value.
    fn record_published(&mut self, task: &Task, commit: &str) -> Result<String> {
        self.recorder.record(
            Some(task.id),
            EventKind::PublishVerified {
                commit: commit.to_string(),
                remote_sha: commit.to_string(),
            },
        )?;
        Ok(commit.to_string())
    }

    /// Runs [`run_completion_set`] against `prep` and records the outcome as
    /// [`EventKind::VerifyPassed`] or [`EventKind::VerifyFailed`] for
    /// `attempt` — the first, journaled run [`Runner::verify_and_publish`]
    /// makes before anything is committed.
    fn run_completion_gates(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
    ) -> Result<()> {
        let results = run_completion_set(&self.profile, &prep.worktree, &prep.base_sha, None)?;
        if completion_passed(&results) {
            self.recorder
                .record(Some(task.id), EventKind::VerifyPassed { attempt })?;
            Ok(())
        } else {
            let (class, detail) = completion_failure(&results);
            self.recorder.record(
                Some(task.id),
                EventKind::VerifyFailed {
                    attempt,
                    class,
                    detail: detail.clone(),
                },
            )?;
            Err(Error::Gate {
                kind: "completion".to_string(),
                detail,
            })
        }
    }

    /// Reruns [`run_completion_set`] against `prep` without journaling
    /// anything: the check [`Runner::retry_after_rebase`] makes after a
    /// clean rebase, where `task`'s pipeline state has already moved past
    /// [`EventKind::VerifyPassed`]/[`EventKind::VerifyFailed`] and cannot
    /// legally accept another one (`state.rs`'s `from_publishing`).
    fn check_completion_gates(&self, prep: &Prepared) -> Result<()> {
        let results = run_completion_set(&self.profile, &prep.worktree, &prep.base_sha, None)?;
        if completion_passed(&results) {
            Ok(())
        } else {
            let (_, detail) = completion_failure(&results);
            Err(Error::Gate {
                kind: "completion".to_string(),
                detail,
            })
        }
    }
}

/// Whether every gate [`run_completion_set`] ran passed — and it ran at
/// least one, since an empty result would otherwise vacuously "pass" without
/// a single gate having proven anything.
fn completion_passed(results: &[GateResult]) -> bool {
    !results.is_empty() && results.iter().all(|result| result.passed)
}

/// Classifies why [`run_completion_set`]'s `results` did not all pass, and
/// names the first gate that failed. `results` is expected to contain at
/// least the mandatory [`GateKind::Verify`] gate ([`profile_from`]
/// guarantees this), so the "no gate ran at all" branch below is only ever
/// reached defensively.
fn completion_failure(results: &[GateResult]) -> (FailureClass, String) {
    let class = classify(&empty_outcome(), results, None);
    let detail = results.iter().find(|result| !result.passed).map_or_else(
        || "the completion set ran no gate to verify".to_string(),
        |result| format!("completion gate {:?} failed", result.kind),
    );
    (class, detail)
}

/// Whether `err` is [`crate::publish`]'s report of a push `remote` itself
/// rejected — `args[0] == "push"`, the shape its own doc comment promises —
/// as distinct from the push succeeding but the post-push fetched-tip
/// comparison mismatching (`args[0] == "publish"`), which
/// [`Runner::verify_and_publish`] does not attempt to recover from
/// automatically.
fn is_rejected_push(err: &Error) -> bool {
    matches!(err, Error::Git { args, .. } if args.first().map(String::as_str) == Some("push"))
}

/// The commit message [`Runner::verify_and_publish`] gives the candidate
/// commit [`commit_all`] produces: `task`'s title, so mainline history reads
/// one line per task the same way `git log --oneline` already would.
fn commit_message(task: &Task) -> String {
    task.title().to_string()
}

/// The empty [`TestSummary`]: no tests run, none failing. [`Runner::gate_phase`]'s
/// baseline when a caller has none yet, and its own result when a `tdd` `Red`
/// phase is skipped by a declared exception.
fn empty_summary() -> TestSummary {
    TestSummary {
        passed: 0,
        failed: 0,
        ignored: 0,
        failures: Vec::new(),
    }
}

/// Restricts `dir` to owner-only access, mirroring [`crate::write_evidence`]'s
/// own directories (`VISION.md` §11). A no-op on non-Unix targets, since
/// there is no equivalent mode bit to set.
#[cfg(unix)]
fn set_private(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Writes `result`'s command, combined output and `tree_hash` as durable,
/// redacted evidence at
/// `<state_dir>/attempts/<task>/<attempt>/gates/<phase>.log`
/// (`VISION.md` §9, §11) — mid-attempt, well before [`write_evidence`] is
/// ever called for `attempt`, so [`Runner::gate_phase`] cannot defer this to
/// it.
///
/// # Errors
///
/// Returns [`Error::Io`] if the directory or file cannot be created or
/// written.
fn write_gate_evidence(
    project: &Project,
    task: TaskId,
    attempt: AttemptId,
    phase: Phase,
    gate: &Gate,
    result: &GateResult,
    tree_hash: &str,
) -> Result<()> {
    let dir = project
        .state_dir
        .join("attempts")
        .join(task.get().to_string())
        .join(attempt.get().to_string())
        .join("gates");
    std::fs::create_dir_all(&dir)?;
    set_private(&dir)?;

    let content = redact(
        &format!(
            "command: {}\ntree_hash: {tree_hash}\n\n{}{}",
            gate.command.join(" "),
            result.stdout,
            result.stderr,
        ),
        &[],
    );
    let name = format!("{phase:?}").to_lowercase();
    std::fs::write(dir.join(format!("{name}.log")), content)?;
    Ok(())
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

/// The [`TaskState`] [`crate::apply`] derives for `task` by replaying every
/// event journaled for it, in order, from [`TaskState::Queued`] — the
/// single source of truth [`Runner::drive_attempt`] returns, rather than a
/// state its own control flow would otherwise merely assume.
///
/// Opens its own read of `project`'s journal, the same pattern
/// [`next_attempt_id`] already uses, rather than going through a
/// [`Runner`]'s [`Recorder`], which exposes no way to read events back.
///
/// # Errors
///
/// Returns whatever [`Journal::open_for`] or [`Journal::events_for`] return
/// on failure to open or read the journal, and [`Error::InvalidTransition`]
/// if any recorded event does not legally apply to the state that preceded
/// it.
fn journaled_state(project: &Project, task: TaskId) -> Result<TaskState> {
    let journal = Journal::open_for(project)?;
    journal
        .events_for(task)?
        .into_iter()
        .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))
}

/// How a run of the queue — or any single CLI command — concluded.
///
/// Every outcome `docs/CONTRACT.md` §1 documents has a variant here.
/// Translating a variant into a process exit status is the CLI's job alone;
/// nothing in this crate reasons about exit statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The queue drained: every task reached a terminal state and none
    /// failed.
    Drained,
    /// `task` failed after exhausting its remediation budget; the queue
    /// never advances past it.
    TaskFailed {
        /// The task that failed.
        task: TaskId,
    },
    /// A command-level diagnostic check failed (`docs/CONTRACT.md` §3
    /// `doctor`: "Exit 0 if every check passes, 1 otherwise"). Unlike
    /// [`RunOutcome::TaskFailed`], this is not about any one queued task, so
    /// it carries a summary of what failed instead of a [`TaskId`]. Maps to
    /// the same exit code as `TaskFailed`: both are "the command ran to
    /// completion and found something wrong," as opposed to the pauses
    /// below, which stop before completion.
    CheckFailed {
        /// Which checks failed, and why.
        detail: String,
    },
    /// The command could not even start: bad arguments, a malformed task
    /// file, or no registered project.
    Usage {
        /// What was wrong.
        detail: String,
    },
    /// A provider usage limit was hit; work is paused, not failed.
    ProviderLimit {
        /// When the limit is expected to lift, if the provider reported
        /// one.
        until: Option<OffsetDateTime>,
    },
    /// `task` is waiting at a human gate; `ktask-rs ack` clears it.
    HumanGate {
        /// The task waiting at the gate.
        task: TaskId,
    },
    /// `task` is waiting on an answered decision; `ktask-rs resolve` clears
    /// it.
    NeedsInput {
        /// The task waiting on the decision.
        task: TaskId,
    },
    /// The run was interrupted (for example by SIGINT); state is durable
    /// and the same run resumes from where it left off.
    Interrupted,
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

/// A synthetic single-element gate list for [`classify()`] and [`bundle`],
/// built from `err` when it is [`Error::Gate`] — [`Runner::verify_and_publish`]'s
/// own error for a failing completion gate carries only a kind and a detail
/// message, not the [`GateResult`] history `classify` otherwise expects.
/// This stands in for it just well enough that `classify` reports
/// [`FailureClass::VerificationFailure`] rather than falling through to its
/// generic [`FailureClass::AgentFailure`] fallback, and that the failing
/// gate's own detail text reaches [`bundle`]'s "Failing gates" section.
///
/// Every other [`Error`] variant [`Runner::verify_and_publish`] can return
/// (`Policy`, `Git`) is already classified correctly from `git_error` alone,
/// without needing a [`GateResult`] at all, so this returns an empty `Vec`
/// for anything but [`Error::Gate`].
/// A short, deterministic summary of everything changed in `prep.worktree`
/// since `prep.base_sha`, for [`bundle`]'s `diff_summary` argument.
///
/// # Errors
///
/// Returns whatever [`changed_paths`] returns on failure to diff.
fn diff_summary(prep: &Prepared) -> Result<String> {
    let changed = changed_paths(&prep.worktree, &prep.base_sha)?;
    if changed.is_empty() {
        return Ok("no files changed".to_string());
    }
    Ok(format!(
        "{} file(s) changed: {}",
        changed.len(),
        changed
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn gates_for_classification(err: &Error) -> Vec<GateResult> {
    match err {
        Error::Gate { detail, .. } => vec![GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: None,
            signal: None,
            duration_ms: 0,
            stdout: String::new(),
            stderr: detail.clone(),
            timed_out: false,
        }],
        _ => Vec::new(),
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
        Bus, Capabilities, Dummy, Error, Event, Phase, Scenario, ScenarioFile, Step, StepOutcome,
        TaskStatus, TddException, WriteScope, project_config_path, read_evidence, report_path,
    };
    use crate::{pending, send};

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

    fn all_outcomes() -> Vec<RunOutcome> {
        vec![
            RunOutcome::Drained,
            RunOutcome::TaskFailed {
                task: TaskId::new(1),
            },
            RunOutcome::CheckFailed {
                detail: "git: not runnable".to_string(),
            },
            RunOutcome::Usage {
                detail: "no registered project".to_string(),
            },
            RunOutcome::ProviderLimit {
                until: Some(OffsetDateTime::UNIX_EPOCH),
            },
            RunOutcome::HumanGate {
                task: TaskId::new(1),
            },
            RunOutcome::NeedsInput {
                task: TaskId::new(1),
            },
            RunOutcome::Interrupted,
        ]
    }

    #[test]
    fn outcome_has_exactly_eight_variants() {
        let variants = all_outcomes();
        assert_eq!(variants.len(), 8);

        // Exhaustive, wildcard-free match: a variant added to `RunOutcome`
        // without being listed here fails to compile instead of silently
        // passing untested.
        for outcome in &variants {
            match outcome {
                RunOutcome::Drained
                | RunOutcome::TaskFailed { .. }
                | RunOutcome::CheckFailed { .. }
                | RunOutcome::Usage { .. }
                | RunOutcome::ProviderLimit { .. }
                | RunOutcome::HumanGate { .. }
                | RunOutcome::NeedsInput { .. }
                | RunOutcome::Interrupted => {}
            }
        }
    }

    #[test]
    fn outcome_provider_limit_carries_no_deadline_when_the_provider_reported_none() {
        let outcome = RunOutcome::ProviderLimit { until: None };

        assert_eq!(outcome, RunOutcome::ProviderLimit { until: None });
        assert_ne!(
            outcome,
            RunOutcome::ProviderLimit {
                until: Some(OffsetDateTime::UNIX_EPOCH)
            }
        );
    }

    #[test]
    fn outcome_task_failed_and_human_gate_and_needs_input_each_name_their_task() {
        assert_eq!(
            RunOutcome::TaskFailed {
                task: TaskId::new(3)
            },
            RunOutcome::TaskFailed {
                task: TaskId::new(3)
            }
        );
        assert_ne!(
            RunOutcome::TaskFailed {
                task: TaskId::new(3)
            },
            RunOutcome::TaskFailed {
                task: TaskId::new(4)
            }
        );
        assert_ne!(
            RunOutcome::HumanGate {
                task: TaskId::new(3)
            },
            RunOutcome::NeedsInput {
                task: TaskId::new(3)
            },
            "a human gate and a needs-input pause on the same task are distinct outcomes"
        );
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
    fn a_subscription_taken_from_the_runner_sees_the_events_it_records() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        write_runnable_config(&project);
        let task = sample_task(1);

        let mut runner = Runner::new(project).expect("build runner");
        let mut subscription = runner.subscribe();
        runner.begin_attempt(&task).expect("begin_attempt");

        let (events, dropped) = subscription.drain();
        assert_eq!(dropped, 0);
        let kinds: Vec<_> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert_eq!(kinds, vec!["AttemptStarted"]);
        assert_eq!(events[0].task_id, Some(task.id));
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
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
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
            interrupt: interrupt_flag(),
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
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
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
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
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
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    fn red_spec() -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Red,
            write_scope: WriteScope::TestsOnly,
            gate: Some(GateKind::Targeted),
            records_evidence: true,
        }
    }

    fn green_spec() -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Green,
            write_scope: WriteScope::All,
            gate: Some(GateKind::Targeted),
            records_evidence: true,
        }
    }

    fn refactor_spec() -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Refactor,
            write_scope: WriteScope::All,
            gate: Some(GateKind::Targeted),
            records_evidence: false,
        }
    }

    fn empty_test_summary() -> TestSummary {
        TestSummary {
            passed: 0,
            failed: 0,
            ignored: 0,
            failures: Vec::new(),
        }
    }

    /// Builds a [`Config`] carrying [`runnable_config`]'s mandatory
    /// `verify_command` plus a `targeted_test_command` of `cat <fixture>`:
    /// the simplest way to hand [`Runner::gate_phase`] fixed, arbitrary
    /// cargo-shaped output without any shell-quoting concern.
    fn config_with_fixture(fixture: &std::path::Path) -> Config {
        let mut config = runnable_config();
        config.targeted_test_command = Some(cat_command(fixture));
        config
    }

    fn cat_command(fixture: &std::path::Path) -> Vec<String> {
        vec!["cat".to_string(), fixture.display().to_string()]
    }

    /// A command that prints `fixture`'s contents and then exits `1`,
    /// passing the path as `sh -c`'s own `$1` rather than interpolating it
    /// into the script text, so the fixture's path never has to be
    /// shell-escaped.
    fn cat_then_fail_command(fixture: &std::path::Path) -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat \"$1\"; exit 1".to_string(),
            "_".to_string(),
            fixture.display().to_string(),
        ]
    }

    fn write_fixture(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write fixture");
        path
    }

    /// `cargo test`-shaped output reporting one genuinely new failure.
    const NEW_FAILURE: &str = "\
running 1 test
test widget::tests::rejects_a_bad_size ... FAILED

failures:

---- widget::tests::rejects_a_bad_size stdout ----
thread panicked

failures:
    widget::tests::rejects_a_bad_size

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    /// `cargo test`-shaped output reporting no failures at all.
    const NO_FAILURES: &str = "\
running 1 test
test widget::tests::already_passing ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    /// `cargo test`-shaped output where the red phase's own test now passes,
    /// but an unrelated test regressed.
    const REGRESSION: &str = "\
running 2 tests
test widget::tests::rejects_a_bad_size ... ok
test other::tests::broke ... FAILED

failures:

---- other::tests::broke stdout ----
thread panicked

failures:
    other::tests::broke

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    /// `cargo test`-shaped output where every test passes.
    const ALL_GREEN: &str = "\
running 1 test
test widget::tests::rejects_a_bad_size ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    #[test]
    fn gate_phase_red_advances_and_journals_gate_started_and_gate_finished_when_a_genuinely_new_failure_appears()
     {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let fixture = write_fixture(state_dir.path(), "red.out", NEW_FAILURE);
        let config = config_with_fixture(&fixture);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let baseline = empty_test_summary();
        let after = runner
            .gate_phase(&prep, &task, attempt, &red_spec(), Some(&baseline))
            .expect("a genuinely new failure must advance red");

        assert_eq!(
            after.failures,
            vec!["widget::tests::rejects_a_bad_size".to_string()]
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert!(
            kinds
                .windows(2)
                .any(|window| window == ["GateStarted", "GateFinished"]),
            "GateStarted must be immediately followed by GateFinished, got {kinds:?}"
        );

        let evidence_path = project
            .state_dir
            .join("attempts")
            .join(task.id.get().to_string())
            .join(attempt.get().to_string())
            .join("gates")
            .join("red.log");
        let evidence = std::fs::read_to_string(&evidence_path).expect("read gate evidence");
        assert!(evidence.contains("command: cat "), "evidence: {evidence}");
        assert!(evidence.contains("tree_hash: "), "evidence: {evidence}");
        assert!(
            evidence.contains("widget::tests::rejects_a_bad_size"),
            "evidence: {evidence}"
        );

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_red_does_not_advance_when_no_new_failing_test_is_produced() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let fixture = write_fixture(state_dir.path(), "red.out", NO_FAILURES);
        let config = config_with_fixture(&fixture);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let baseline = empty_test_summary();
        let err = runner
            .gate_phase(&prep, &task, attempt, &red_spec(), Some(&baseline))
            .expect_err("no new failure must not advance red");

        match err {
            Error::Gate { kind, .. } => assert_eq!(kind, "red"),
            other => panic!("expected Error::Gate, got {other:?}"),
        }

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_green_fails_naming_the_regressed_test_when_it_regresses_another_test() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let fixture = write_fixture(state_dir.path(), "green.out", REGRESSION);
        let config = config_with_fixture(&fixture);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let red_result = TestSummary {
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: vec!["widget::tests::rejects_a_bad_size".to_string()],
        };
        let err = runner
            .gate_phase(&prep, &task, attempt, &green_spec(), Some(&red_result))
            .expect_err("a regression must fail green");

        match err {
            Error::Gate { kind, detail } => {
                assert_eq!(kind, "green");
                assert!(
                    detail.contains("other::tests::broke"),
                    "detail was: {detail}"
                );
            }
            other => panic!("expected Error::Gate, got {other:?}"),
        }

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_green_advances_when_every_expected_test_now_passes() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let fixture = write_fixture(state_dir.path(), "green.out", ALL_GREEN);
        let config = config_with_fixture(&fixture);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let red_result = TestSummary {
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: vec!["widget::tests::rejects_a_bad_size".to_string()],
        };
        let after = runner
            .gate_phase(&prep, &task, attempt, &green_spec(), Some(&red_result))
            .expect("every expected test passing must advance green");

        assert!(after.failures.is_empty());

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_skips_red_and_records_tdd_exception_used_when_the_task_declares_one() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let mut config = runnable_config();
        // A gate command that would fail to spawn if `gate_phase` ever ran
        // it, proving the exception path skips the gate entirely rather
        // than merely ignoring its result.
        config.targeted_test_command = Some(vec!["ktask-gate-phase-must-not-run".to_string()]);
        let mut task = sample_task(1);
        task.body = "Do the thing\n\n\
             **TDD-Exception:** documentation: README only, no code changed.\n"
            .to_string();
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let after = runner
            .gate_phase(&prep, &task, attempt, &red_spec(), None)
            .expect("a declared exception must skip red rather than error");

        assert_eq!(after, empty_test_summary());

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert!(
            kinds.contains(&"TddExceptionUsed"),
            "expected TddExceptionUsed among {kinds:?}"
        );
        assert!(
            !kinds.contains(&"GateStarted"),
            "the gate must never run when an exception is declared, got {kinds:?}"
        );

        let recorded = events
            .iter()
            .find(|event| event.kind.discriminant() == "TddExceptionUsed")
            .expect("TddExceptionUsed event")
            .kind
            .clone();
        let EventKind::TddExceptionUsed { exception, reason } = recorded else {
            panic!("expected TddExceptionUsed");
        };
        assert_eq!(exception, TddException::Documentation);
        assert_eq!(reason, "README only, no code changed.");

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_for_a_non_red_green_phase_fails_when_the_gate_command_itself_fails() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let fixture = write_fixture(state_dir.path(), "refactor.out", ALL_GREEN);
        let mut config = runnable_config();
        config.targeted_test_command = Some(cat_then_fail_command(&fixture));
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let err = runner
            .gate_phase(&prep, &task, attempt, &refactor_spec(), None)
            .expect_err("a failing gate command must fail a non-red/green phase");

        match err {
            Error::Gate { detail, .. } => assert!(detail.contains("gate command failed")),
            other => panic!("expected Error::Gate, got {other:?}"),
        }

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_for_a_non_red_green_phase_advances_when_the_gate_command_passes() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let fixture = write_fixture(state_dir.path(), "refactor.out", ALL_GREEN);
        let config = config_with_fixture(&fixture);
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let after = runner
            .gate_phase(&prep, &task, attempt, &refactor_spec(), None)
            .expect("a passing gate command must advance a non-red/green phase");

        assert!(after.failures.is_empty());

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_fails_with_a_policy_error_when_the_phase_names_no_gate() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let config = runnable_config();
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let err = runner
            .gate_phase(&prep, &task, attempt, &implement_spec(), None)
            .expect_err("a phase with no configured gate must be rejected");

        assert!(matches!(err, Error::Policy { .. }));

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn gate_phase_fails_with_a_config_error_when_the_named_gate_has_no_command_configured() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let config = runnable_config();
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let err = runner
            .gate_phase(&prep, &task, attempt, &red_spec(), None)
            .expect_err("a gate kind with no configured command must be rejected");

        assert!(matches!(err, Error::Config { .. }));

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    /// Every event journaled for `task` so far, in order —
    /// [`Runner::prepare`]'s own `PreflightStarted`/`PreflightPassed` pair
    /// always leads, since every `verify_and_publish` test below reaches
    /// [`Runner::verify_and_publish`] through a real [`Runner::prepare`] call.
    fn verify_and_publish_events(project: &Project, task: TaskId) -> Vec<Event> {
        let journal = Journal::open_for(project).expect("open journal");
        journal.events_for(task).expect("events_for")
    }

    fn verify_and_publish_discriminants(project: &Project, task: TaskId) -> Vec<&'static str> {
        verify_and_publish_events(project, task)
            .iter()
            .map(|event| event.kind.discriminant())
            .collect()
    }

    #[test]
    fn verify_and_publish_commits_and_publishes_a_clean_worktree_with_passing_gates() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, runnable_config(), Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let published = runner
            .verify_and_publish(&prep, &task, attempt)
            .expect("verify_and_publish");

        assert_eq!(
            published, repo.seed_sha,
            "nothing changed beyond the fetched base, so the published commit is that base"
        );
        let remote_tip =
            crate::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
        assert_eq!(remote_tip, published);

        assert_eq!(
            verify_and_publish_discriminants(&project, task.id),
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "VerifyPassed",
                "PublishStarted",
                "PublishVerified",
            ]
        );

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn verify_and_publish_rejects_a_dirty_worktree_as_a_policy_failure_before_any_gate_runs() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, runnable_config(), Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");
        std::fs::write(prep.worktree.join("stray.txt"), "uncommitted\n")
            .expect("leave the worktree dirty");

        let err = runner
            .verify_and_publish(&prep, &task, attempt)
            .expect_err("a dirty worktree must never reach publication");

        assert!(matches!(err, Error::Policy { .. }), "got {err:?}");

        let events = verify_and_publish_events(&project, task.id);
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.discriminant())
                .collect::<Vec<_>>(),
            vec!["PreflightStarted", "PreflightPassed", "VerifyFailed"]
        );
        let EventKind::VerifyFailed { class, .. } = &events[2].kind else {
            panic!("expected VerifyFailed, got {:?}", events[2].kind);
        };
        assert_eq!(*class, FailureClass::PolicyFailure);

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn verify_and_publish_rejects_a_failing_completion_gate_before_anything_is_committed() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut config = runnable_config();
        config.verify_command = Some(vec!["false".to_string()]);
        let mut runner = manual_runner(&project, config, Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        let err = runner
            .verify_and_publish(&prep, &task, attempt)
            .expect_err("a failing completion gate must never reach publication");

        assert!(matches!(err, Error::Gate { .. }), "got {err:?}");

        let events = verify_and_publish_events(&project, task.id);
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.discriminant())
                .collect::<Vec<_>>(),
            vec!["PreflightStarted", "PreflightPassed", "VerifyFailed"]
        );
        let EventKind::VerifyFailed { class, .. } = &events[2].kind else {
            panic!("expected VerifyFailed, got {:?}", events[2].kind);
        };
        assert_eq!(*class, FailureClass::VerificationFailure);

        assert_eq!(
            head_sha(&repo.origin).expect("head_sha of bare origin"),
            repo.seed_sha,
            "origin must be untouched"
        );

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn verify_and_publish_recovers_a_rejected_push_from_a_clean_divergence_by_rebasing_rerunning_gates_and_retrying_once()
     {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, runnable_config(), Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        // Another actor's commit lands on `origin` after this attempt's
        // worktree was created from the seed, but before this attempt
        // publishes — the same shape `ScratchRepo::diverge` builds,
        // reproduced here because the divergent commit must live only on
        // `origin`, never in `repo.path`'s own local checkout.
        let shadow = tempfile::tempdir().expect("shadow tempdir");
        crate::git(
            shadow.path(),
            &["clone", "--quiet", &repo.origin.to_string_lossy(), "."],
        )
        .expect("clone shadow");
        std::fs::write(shadow.path().join("origin-only.txt"), "origin change\n")
            .expect("write origin-only.txt");
        crate::git(shadow.path(), &["add", "origin-only.txt"]).expect("git add");
        crate::git(
            shadow.path(),
            &["commit", "--quiet", "-m", "add origin-only.txt"],
        )
        .expect("git commit");
        crate::git(
            shadow.path(),
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        )
        .expect("push origin-only.txt");
        let origin_sha =
            crate::git(shadow.path(), &["rev-parse", "HEAD"]).expect("rev-parse shadow HEAD");

        // The agent's own work, already committed inside the attempt's
        // worktree, on disjoint paths from the origin-only commit above — a
        // clean divergence the rebase can replay without a conflict.
        std::fs::write(prep.worktree.join("local-only.txt"), "local change\n")
            .expect("write local-only.txt");
        crate::git(&prep.worktree, &["add", "local-only.txt"]).expect("git add");
        crate::git(
            &prep.worktree,
            &["commit", "--quiet", "-m", "add local-only.txt"],
        )
        .expect("git commit");
        let local_sha = head_sha(&prep.worktree).expect("head_sha before publish");

        let published = runner
            .verify_and_publish(&prep, &task, attempt)
            .expect("a clean divergence must recover mechanically");

        assert_ne!(
            published, local_sha,
            "a rebased candidate must carry a new sha, replayed onto the new base"
        );
        let remote_tip =
            crate::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
        assert_eq!(remote_tip, published);
        let parent = crate::git(&prep.worktree, &["rev-parse", &format!("{published}^")])
            .expect("rev-parse rebased commit's parent");
        assert_eq!(
            parent, origin_sha,
            "the rebased commit must sit on top of the fetched remote tip"
        );

        // Exactly one VerifyPassed and one PublishStarted despite the
        // rebase-and-retry: `task`'s pipeline state moved to `Publishing` on
        // the first `PublishStarted` and cannot legally accept another one
        // (`state.rs`'s `from_publishing`).
        assert_eq!(
            verify_and_publish_discriminants(&project, task.id),
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "VerifyPassed",
                "PublishStarted",
                "PublishVerified",
            ]
        );

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    #[test]
    fn verify_and_publish_stops_a_conflicting_divergence_with_a_git_error_rather_than_retrying() {
        let repo = scratch_repo().expect("scratch_repo");
        repo.commit("shared.txt", "base\n")
            .expect("commit shared.txt");
        crate::git(
            &repo.path,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        )
        .expect("push shared.txt to origin");

        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let mut runner = manual_runner(&project, runnable_config(), Box::new(AlwaysAvailable));
        let prep = runner.prepare(&task).expect("prepare");

        // The agent's own work modifies `shared.txt` in the attempt's
        // worktree...
        std::fs::write(prep.worktree.join("shared.txt"), "local change\n")
            .expect("write shared.txt locally");
        crate::git(&prep.worktree, &["add", "shared.txt"]).expect("git add");
        crate::git(&prep.worktree, &["commit", "--quiet", "-m", "local edit"]).expect("git commit");
        let local_sha = head_sha(&prep.worktree).expect("head_sha before publish");

        // ...while another actor's commit modifies the very same path on
        // `origin`, guaranteeing the rebase this triggers conflicts.
        let shadow = tempfile::tempdir().expect("shadow tempdir");
        crate::git(
            shadow.path(),
            &["clone", "--quiet", &repo.origin.to_string_lossy(), "."],
        )
        .expect("clone shadow");
        std::fs::write(shadow.path().join("shared.txt"), "origin change\n")
            .expect("write shared.txt on origin");
        crate::git(shadow.path(), &["add", "shared.txt"]).expect("git add");
        crate::git(shadow.path(), &["commit", "--quiet", "-m", "origin edit"]).expect("git commit");
        crate::git(
            shadow.path(),
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        )
        .expect("push origin edit");

        let err = runner
            .verify_and_publish(&prep, &task, attempt)
            .expect_err("a conflicting divergence must never be resolved automatically");

        let Error::Git { args, stderr } = &err else {
            panic!("expected Error::Git, got {err:?}");
        };
        assert_eq!(args, &["rebase".to_string(), "conflict".to_string()]);
        assert!(
            stderr.contains("shared.txt"),
            "detail must name the conflicted path, got {stderr:?}"
        );

        // No agent was called and no further publish was attempted: the
        // worktree is exactly where the agent's own commit left it, and no
        // rebase is mid-flight.
        assert_eq!(
            head_sha(&prep.worktree).expect("head_sha after the aborted rebase"),
            local_sha
        );
        assert!(!prep.worktree.join(".git").join("rebase-merge").exists());
        assert!(!prep.worktree.join(".git").join("rebase-apply").exists());

        assert_eq!(
            verify_and_publish_discriminants(&project, task.id),
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "VerifyPassed",
                "PublishStarted",
            ]
        );

        let worktree = prep.worktree.clone();
        drop(prep);
        remove_worktree(&repo.path, &worktree).expect("clean up the created worktree");
    }

    /// A two-step dummy scenario: the first step answers
    /// [`check_provider_available`]'s empty-prompt probe during
    /// [`Runner::prepare`]'s preflight, and the second answers the `direct`
    /// protocol's single `Implement` phase, writing a `KTASK_RESULT: DONE`
    /// report at `report_path` and touching nothing else — so the worktree
    /// [`Runner::verify_and_publish`] checks stays clean, exactly like
    /// [`write_config_with_an_available_dummy_provider`]'s scenario but with
    /// the second step present for `run_phase` to consume.
    fn write_config_for_a_successful_direct_run(project: &Project, report_path: &std::path::Path) {
        let state_dir = project.state_dir.clone();
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
                    stdout: Some("implemented the thing\n".to_string()),
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: report_path.to_path_buf(),
                        content: "KTASK_RESULT: DONE\nSummary: it worked.\n".to_string(),
                    }],
                },
            ],
        };
        let scenario_path = state_dir.join("scenario.toml");
        std::fs::write(&scenario_path, scenario.to_toml().expect("serialize"))
            .expect("write scenario");
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
    fn run_task_drives_a_dummy_success_through_to_done_leaving_no_worktree_or_lock_behind() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let expected_report_path = report_path(&project, task.id, AttemptId::new(1));
        write_config_for_a_successful_direct_run(&project, &expected_report_path);

        let mut runner = Runner::new(project.clone()).expect("build runner");
        let state = runner.run_task(&task).expect("run_task");

        assert_eq!(state, TaskState::Done);

        let worktrees = crate::list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(
            worktrees.len(),
            1,
            "only the main worktree may remain, got {worktrees:?}"
        );

        drop(
            acquire(&project.state_dir, Duration::from_secs(0))
                .expect("run_task must release the repository lock on success"),
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert_eq!(
            kinds,
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "AttemptStarted",
                "PhaseEntered",
                "AgentOutput",
                "AttemptFinished",
                "PhaseEntered",
                "VerifyPassed",
                "PublishStarted",
                "PublishVerified",
                "TaskDone",
            ]
        );

        let EventKind::TaskDone { commit } = &events[10].kind else {
            panic!("expected TaskDone, got {:?}", events[10].kind);
        };
        let remote_tip =
            crate::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
        assert_eq!(*commit, remote_tip);
    }

    #[test]
    fn run_task_removes_the_worktree_and_releases_the_lock_when_a_step_fails_partway() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);

        // Two steps, neither of which writes a report file: one for
        // `prepare`'s own availability check, one for `run_phase`'s
        // `Implement` invocation — which therefore fails to find the report
        // it was told to write (`run_phase_fails_with_a_classified_report_error_when_the_agent_never_writes_one`'s
        // own fixture, reused here as `run_task`'s partway failure).
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
        let err = runner
            .run_task(&task)
            .expect_err("a missing report must fail run_task rather than being assumed");

        assert!(matches!(err, Error::Report { .. }), "got {err:?}");

        let worktrees = crate::list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(
            worktrees.len(),
            1,
            "the failed attempt's worktree must still be cleaned up, got {worktrees:?}"
        );

        drop(
            acquire(&project.state_dir, Duration::from_secs(0))
                .expect("run_task must release the repository lock even when a step fails partway"),
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert_eq!(
            kinds,
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "AttemptStarted",
                "PhaseEntered",
                "AttemptFinished",
                "TaskFailed",
            ],
            "the failed attempt ends in TaskFailed, never TaskDone"
        );
        assert!(
            matches!(
                journaled_state(&project, task.id).expect("state"),
                TaskState::Failed {
                    class: FailureClass::AgentFailure,
                    ..
                }
            ),
            "a failed attempt must leave the task Failed, not stranded mid-attempt"
        );
    }

    #[test]
    fn run_task_pauses_a_human_gate_task_without_ever_invoking_the_provider() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let mut task = sample_task(1);
        task.status = TaskStatus::HumanGate;

        // No steps at all: any call to the provider (preflight's own probe
        // included) would fail the scenario as exhausted, so a passing test
        // proves the provider is never reached.
        let scenario = Scenario { steps: Vec::new() };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let state = runner
            .run_task(&task)
            .expect("a human gate must pause, never fail");

        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::HumanGate,
                resume_to: Box::new(TaskState::Queued),
            }
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert_eq!(
            kinds,
            vec!["Paused"],
            "a gate is never handed to preflight or an agent"
        );

        let worktrees = crate::list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(
            worktrees.len(),
            1,
            "a human gate creates no worktree to begin with, got {worktrees:?}"
        );

        // Resumable exactly the documented way: only `ack` (`GateAcknowledged`)
        // resolves a `HumanGate` pause, never `Resumed`.
        assert!(apply(&state, &EventKind::Resumed).is_err());
        let acknowledged = apply(
            &state,
            &EventKind::GateAcknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
        )
        .expect("a HumanGate pause accepts GateAcknowledged");
        assert!(matches!(acknowledged, TaskState::Acknowledged { .. }));
    }

    #[test]
    fn run_task_pauses_with_a_decision_request_when_the_agent_reports_needs_input() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);
        let expected_report_path = report_path(&project, task.id, attempt);

        let scenario = Scenario {
            steps: vec![
                // `prepare`'s own `check_provider_available` probe.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: Vec::new(),
                },
                // The `Implement` phase: the agent cannot proceed and asks a
                // structured question instead of claiming done or failed.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::NeedsInput,
                    stdout: Some("which database driver?".to_string()),
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: expected_report_path,
                        content: "KTASK_RESULT: NEEDS_INPUT\n\
                                  Question: Postgres or SQLite for the journal?\n\
                                  Options:\n\
                                  - Postgres\n\
                                  - SQLite\n\
                                  Trade-offs: Postgres scales better; SQLite is simpler to run.\n\
                                  Impact: Journal durability and operational overhead.\n\
                                  Recommended: SQLite\n"
                            .to_string(),
                    }],
                },
            ],
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let state = runner
            .run_task(&task)
            .expect("a needs-input report must pause, never fail");

        let TaskState::Paused {
            reason: PauseReason::Input,
            resume_to,
        } = state.clone()
        else {
            panic!("expected an Input pause, got {state:?}");
        };
        assert_eq!(
            *resume_to,
            TaskState::Running {
                attempt,
                phase: Phase::Implement,
            }
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert_eq!(
            kinds,
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "AttemptStarted",
                "PhaseEntered",
                "AgentOutput",
                "AttemptFinished",
                "DecisionRaised",
            ],
            "the mandatory completion gates must never run before a decision is answered"
        );

        let EventKind::DecisionRaised { request } = &events[6].kind else {
            panic!("expected DecisionRaised, got {:?}", events[6].kind);
        };
        assert_eq!(request.question, "Postgres or SQLite for the journal?");

        // Resumable exactly the documented way: `Resumed` returns to the
        // interrupted phase; `GateAcknowledged` does not apply here.
        assert!(matches!(
            apply(&state, &EventKind::Resumed).expect("an Input pause accepts Resumed"),
            TaskState::Running { .. }
        ));

        let worktrees = crate::list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(
            worktrees.len(),
            1,
            "the worktree must still be cleaned up, got {worktrees:?}"
        );
    }

    #[test]
    fn run_task_pauses_with_a_wait_deadline_when_the_provider_reports_a_usage_limit() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let attempt = AttemptId::new(1);

        let scenario = Scenario {
            steps: vec![
                // `prepare`'s own `check_provider_available` probe.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: Vec::new(),
                },
                // The `Implement` phase: the provider hits a usage limit and
                // writes no report at all -- exactly what a real provider
                // stopped mid-run by a limit would leave behind.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Limit,
                    stdout: Some("usage limit reached, try again in 20s".to_string()),
                    exit_code: Some(1),
                    delay_ms: None,
                    files: Vec::new(),
                },
            ],
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let state = runner
            .run_task(&task)
            .expect("a provider usage limit must pause, never fail");

        let TaskState::Paused {
            reason: PauseReason::Limit { until },
            resume_to,
        } = state.clone()
        else {
            panic!("expected a Limit pause, got {state:?}");
        };
        assert!(
            until.is_some(),
            "a recognized reset (\"try again in 20s\") must produce a wait deadline"
        );
        assert_eq!(
            *resume_to,
            TaskState::Running {
                attempt,
                phase: Phase::Implement,
            }
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.discriminant()).collect();
        assert_eq!(
            kinds,
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "AttemptStarted",
                "PhaseEntered",
                "AgentOutput",
                "AttemptFinished",
                "Paused",
            ],
            "a provider limit must never be TaskFailed"
        );

        // Resumable exactly the documented way: `Resumed` returns to the
        // interrupted phase.
        assert!(matches!(
            apply(&state, &EventKind::Resumed).expect("a Limit pause accepts Resumed"),
            TaskState::Running { .. }
        ));

        let worktrees = crate::list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(
            worktrees.len(),
            1,
            "the worktree must still be cleaned up, got {worktrees:?}"
        );
    }

    /// A `verify_command` that fails exactly once: it touches `marker` and
    /// exits `1` the first time it runs, then exits `0` on every call after,
    /// since `marker` now exists. Proves a remediation round's completion
    /// gates are genuinely rerun rather than replaying a cached result — the
    /// second run must actually execute the command to observe `marker` and
    /// pass, not merely skip re-checking.
    fn fails_once_then_passes_command(marker: &std::path::Path) -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "test -f \"$1\" && exit 0 || { touch \"$1\"; exit 1; }".to_string(),
            "_".to_string(),
            marker.display().to_string(),
        ]
    }

    #[test]
    fn run_task_remediates_a_failing_completion_gate_once_then_reaches_done_with_two_attempt_records()
     {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let first_attempt = AttemptId::new(1);
        let retry_attempt = AttemptId::new(2);
        let first_report = report_path(&project, task.id, first_attempt);
        let retry_report = report_path(&project, task.id, retry_attempt);
        let marker = state_dir.path().join("verify-marker");

        let scenario = Scenario {
            steps: vec![
                // `prepare`'s own `check_provider_available` probe.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: Vec::new(),
                },
                // The original `Implement` phase: reports done, but the
                // completion gate below still fails this first time.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("implemented the thing\n".to_string()),
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: first_report,
                        content: "KTASK_RESULT: DONE\nSummary: first pass.\n".to_string(),
                    }],
                },
                // The remediation round's own invocation.
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: Some("fixed it on retry\n".to_string()),
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: retry_report,
                        content: "KTASK_RESULT: DONE\nSummary: fixed on retry.\n".to_string(),
                    }],
                },
            ],
        };

        let mut config = runnable_config();
        config.verify_command = Some(fails_once_then_passes_command(&marker));
        let mut runner = manual_runner(&project, config, Box::new(Dummy::new(scenario)));

        let state = runner
            .run_task(&task)
            .expect("a failure remediated once must still reach Done");

        assert_eq!(state, TaskState::Done);
        assert!(
            marker.exists(),
            "the completion gate must have actually run"
        );

        let records = read_evidence(&project, task.id).expect("read_evidence");
        assert_eq!(
            records.len(),
            2,
            "the original failed attempt and the remediated retry must both be evidenced, got {records:?}"
        );
        assert_eq!(records[0].id, first_attempt);
        assert_eq!(records[1].id, retry_attempt);
        assert!(
            records[0].exit_reason.contains("VerificationFailure"),
            "the first attempt's evidence must record why it failed, got {:?}",
            records[0].exit_reason
        );
        assert!(records[0].candidate_sha.is_none());
        assert!(records[1].candidate_sha.is_some());
        assert!(
            records[1].gates.is_empty(),
            "no stale gate result from the first attempt may survive into the second, got {:?}",
            records[1].gates
        );

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(task.id).expect("events_for");
        let verify_failed_count = events
            .iter()
            .filter(|event| event.kind.discriminant() == "VerifyFailed")
            .count();
        let verify_passed_count = events
            .iter()
            .filter(|event| event.kind.discriminant() == "VerifyPassed")
            .count();
        assert_eq!(
            verify_failed_count, 1,
            "exactly the first attempt's completion gate must have failed"
        );
        assert_eq!(
            verify_passed_count, 1,
            "exactly the remediated retry's completion gate must have passed"
        );

        let EventKind::TaskDone { commit } = &events.last().expect("at least one event").kind
        else {
            panic!(
                "expected the run to end in TaskDone, got {:?}",
                events.last()
            );
        };
        let remote_tip =
            crate::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
        assert_eq!(*commit, remote_tip);
    }

    #[test]
    fn remediate_stops_after_max_remediation_attempts_and_returns_the_last_error() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let first_report = report_path(&project, task.id, AttemptId::new(1));
        let retry_report = report_path(&project, task.id, AttemptId::new(2));

        // `verify_command` always fails: remediation gets no genuine chance
        // to recover, so it must stop after exactly one retry
        // (`max_remediation_attempts`'s default) rather than looping forever.
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
                    files: vec![ScenarioFile {
                        path: first_report,
                        content: "KTASK_RESULT: DONE\nSummary: first pass.\n".to_string(),
                    }],
                },
                Step {
                    on_task: None,
                    on_attempt: None,
                    outcome: StepOutcome::Success,
                    stdout: None,
                    exit_code: Some(0),
                    delay_ms: None,
                    files: vec![ScenarioFile {
                        path: retry_report,
                        content: "KTASK_RESULT: DONE\nSummary: still broken.\n".to_string(),
                    }],
                },
            ],
        };

        let mut config = runnable_config();
        config.verify_command = Some(vec!["false".to_string()]);
        let mut runner = manual_runner(&project, config, Box::new(Dummy::new(scenario)));

        let err = runner
            .run_task(&task)
            .expect_err("an unrecoverable failure must not loop forever");

        assert!(matches!(err, Error::Gate { .. }), "got {err:?}");

        let records = read_evidence(&project, task.id).expect("read_evidence");
        assert_eq!(
            records.len(),
            2,
            "the original attempt and exactly one bounded retry must both be evidenced, got {records:?}"
        );
        assert!(records.iter().all(|record| record.candidate_sha.is_none()));
    }

    /// Answers [`Runner::prepare`]'s own `check_provider_available` probe:
    /// present once per task ahead of that task's own phase step, exactly
    /// like [`write_config_for_a_successful_direct_run`]'s first step.
    fn probe_step() -> Step {
        Step {
            on_task: None,
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: None,
            exit_code: Some(0),
            delay_ms: None,
            files: Vec::new(),
        }
    }

    /// Answers a task's `Implement` phase by writing a `KTASK_RESULT: DONE`
    /// report at `report_path` and touching nothing else, so the worktree
    /// stays clean for [`Runner::verify_and_publish`].
    fn success_step(report_path: &std::path::Path) -> Step {
        Step {
            on_task: None,
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: Some("implemented the thing\n".to_string()),
            exit_code: Some(0),
            delay_ms: None,
            files: vec![ScenarioFile {
                path: report_path.to_path_buf(),
                content: "KTASK_RESULT: DONE\nSummary: it worked.\n".to_string(),
            }],
        }
    }

    /// Answers a task's `Implement` phase without writing a report at all,
    /// so [`crate::read_report`] fails it with [`Error::Report`] — the same
    /// classified failure
    /// [`run_task_removes_the_worktree_and_releases_the_lock_when_a_step_fails_partway`]
    /// relies on.
    fn no_report_step() -> Step {
        Step {
            on_task: None,
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: None,
            exit_code: Some(0),
            delay_ms: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn run_queue_drains_three_dummy_tasks_in_order() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2), sample_task(3)];

        let scenario = Scenario {
            steps: tasks
                .iter()
                .flat_map(|task| {
                    let report = report_path(&project, task.id, AttemptId::new(1));
                    vec![probe_step(), success_step(&report)]
                })
                .collect(),
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
        for task in &tasks {
            let journal = Journal::open_for(&project).expect("open journal");
            let events = journal.events_for(task.id).expect("events_for");
            assert!(
                events
                    .iter()
                    .any(|event| event.kind.discriminant() == "TaskDone"),
                "task {} must have reached TaskDone, events: {:?}",
                task.id,
                events
                    .iter()
                    .map(|e| e.kind.discriminant())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn run_queue_stops_at_a_failing_middle_task_leaving_the_third_untouched() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2), sample_task(3)];
        let first_report = report_path(&project, tasks[0].id, AttemptId::new(1));

        // Task 1 succeeds; task 2's `Implement` phase never writes a
        // report, failing `run_task` outright. No steps are provided beyond
        // that: if task 3 were ever touched, the scenario would fail as
        // exhausted rather than this test passing.
        let scenario = Scenario {
            steps: vec![
                probe_step(),
                success_step(&first_report),
                probe_step(),
                no_report_step(),
            ],
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::TaskFailed { task: tasks[1].id });

        let journal = Journal::open_for(&project).expect("open journal");
        assert!(
            journal
                .events_for(tasks[2].id)
                .expect("events_for")
                .is_empty(),
            "the third task must never have been touched"
        );
        let second_kinds: Vec<&str> = journal
            .events_for(tasks[1].id)
            .expect("events_for")
            .iter()
            .map(|event| event.kind.discriminant())
            .collect();
        assert_eq!(
            second_kinds,
            vec![
                "PreflightStarted",
                "PreflightPassed",
                "AttemptStarted",
                "PhaseEntered",
                "AttemptFinished",
                "TaskFailed",
            ],
            "the failing task ends in TaskFailed and never reaches TaskDone"
        );
    }

    #[test]
    fn run_queue_stops_at_a_human_gate_without_ever_touching_the_task_behind_it() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let mut gate = sample_task(1);
        gate.status = TaskStatus::HumanGate;
        let tasks = vec![gate, sample_task(2)];

        // No steps at all: a human gate is decided before the provider is
        // ever invoked, and task 2 must never be reached either.
        let scenario = Scenario { steps: Vec::new() };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::HumanGate { task: tasks[0].id });
        let journal = Journal::open_for(&project).expect("open journal");
        assert!(
            journal
                .events_for(tasks[1].id)
                .expect("events_for")
                .is_empty(),
            "a task behind an unacknowledged gate must never be touched"
        );
    }

    #[test]
    fn run_queue_from_skips_earlier_tasks_entirely() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2)];
        let second_report = report_path(&project, tasks[1].id, AttemptId::new(1));

        // A single task's worth of steps: if task 1 were considered at all
        // (it never had a `TaskQueued` event, so `next_runnable` would
        // otherwise treat it as blocking task 2's predecessor check), this
        // scenario would be exhausted trying to run it first.
        let scenario = Scenario {
            steps: vec![probe_step(), success_step(&second_report)],
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let outcome = runner
            .run_queue(&tasks, Some(tasks[1].id))
            .expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
        let journal = Journal::open_for(&project).expect("open journal");
        assert!(
            journal
                .events_for(tasks[0].id)
                .expect("events_for")
                .is_empty(),
            "a task before `from` must never be touched"
        );
        assert!(
            journal
                .events_for(tasks[1].id)
                .expect("events_for")
                .iter()
                .any(|event| event.kind.discriminant() == "TaskDone"),
            "the task at `from` must still run to completion"
        );
    }

    #[test]
    fn run_queue_on_an_empty_task_list_is_immediately_drained() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let scenario = Scenario { steps: Vec::new() };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(Dummy::new(scenario)));

        let outcome = runner.run_queue(&[], None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
    }

    /// A [`Provider`] that sends `request` the moment its `on_call`-th
    /// invocation begins, as an operator in another terminal would while
    /// that invocation is in flight, then hands the call to an inner
    /// [`Dummy`]. Calls are counted from 1, and the first of every task is
    /// [`Runner::prepare`]'s availability probe.
    struct Signaller {
        inner: Dummy,
        project: Project,
        request: Request,
        on_call: usize,
        calls: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Provider for Signaller {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == self.on_call {
                send(&self.project, self.request).expect("send the request");
            }
            self.inner.invoke(inv, bus)
        }
    }

    /// A runner over `tasks` whose provider answers every task's probe and
    /// `Implement` phase successfully and sends `request` during the
    /// `on_call`-th invocation.
    fn signalled_runner(
        project: &Project,
        tasks: &[Task],
        request: Request,
        on_call: usize,
    ) -> Runner {
        let scenario = Scenario {
            steps: tasks
                .iter()
                .flat_map(|task| {
                    let report = report_path(project, task.id, AttemptId::new(1));
                    vec![probe_step(), success_step(&report)]
                })
                .collect(),
        };
        let provider = Signaller {
            inner: Dummy::new(scenario),
            project: project.clone(),
            request,
            on_call,
            calls: std::rc::Rc::default(),
        };
        manual_runner(project, runnable_config(), Box::new(provider))
    }

    fn state_of(project: &Project, task: &Task) -> TaskState {
        journaled_state(project, task.id).expect("journaled_state")
    }

    #[test]
    fn a_pause_sent_during_a_task_lets_it_finish_then_parks_the_next_task_and_stops_the_queue() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2), sample_task(3)];
        // Call 2 is task 1's `Implement` phase.
        let mut runner = signalled_runner(&project, &tasks, Request::Pause, 2);

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Interrupted);
        assert_eq!(state_of(&project, &tasks[0]), TaskState::Done);
        assert_eq!(
            state_of(&project, &tasks[1]),
            TaskState::Paused {
                reason: PauseReason::Blocked,
                resume_to: Box::new(TaskState::Queued),
            }
        );
        assert_eq!(state_of(&project, &tasks[2]), TaskState::Queued);
        assert!(
            !pending(&project, Request::Pause),
            "the request is consumed once it has been journaled"
        );
    }

    #[test]
    fn running_the_queue_again_resumes_a_task_the_operator_paused() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2)];
        let mut runner = signalled_runner(&project, &tasks, Request::Pause, 2);
        assert_eq!(
            runner.run_queue(&tasks, None).expect("first run"),
            RunOutcome::Interrupted
        );

        // Task 1 is done, so the second run's scenario needs task 2's steps
        // only.
        let report = report_path(&project, tasks[1].id, AttemptId::new(1));
        let mut second = manual_runner(
            &project,
            runnable_config(),
            Box::new(Dummy::new(Scenario {
                steps: vec![probe_step(), success_step(&report)],
            })),
        );
        let outcome = second.run_queue(&tasks, None).expect("second run");

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, &tasks[1]), TaskState::Done);
        let journal = Journal::open_for(&project).expect("journal");
        let kinds: Vec<&str> = journal
            .events_for(tasks[1].id)
            .expect("events")
            .iter()
            .map(|event| event.kind.discriminant())
            .take(3)
            .collect();
        assert_eq!(kinds, vec!["Paused", "Resumed", "PreflightStarted"]);
    }

    #[test]
    fn a_pause_sent_during_the_last_task_lets_the_queue_drain() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1)];
        let mut runner = signalled_runner(&project, &tasks, Request::Pause, 2);

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, &tasks[0]), TaskState::Done);
    }

    #[test]
    fn requests_left_over_from_an_earlier_supervisor_do_not_affect_a_new_run() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2)];
        send(&project, Request::Pause).expect("stale pause");
        send(&project, Request::Interrupt).expect("stale interrupt");
        send(&project, Request::Cancel(tasks[0].id)).expect("stale cancel");
        // Never fires: only two tasks' worth of calls exist.
        let mut runner = signalled_runner(&project, &tasks, Request::Pause, 99);

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, &tasks[0]), TaskState::Done);
        assert_eq!(state_of(&project, &tasks[1]), TaskState::Done);
    }

    #[test]
    fn an_interrupt_sent_during_a_phase_journals_interrupted_and_stops_the_queue() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2)];
        let mut runner = signalled_runner(&project, &tasks, Request::Interrupt, 2);

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Interrupted);
        assert_eq!(
            state_of(&project, &tasks[0]),
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                }),
            }
        );
        assert_eq!(state_of(&project, &tasks[1]), TaskState::Queued);
        assert!(
            !pending(&project, Request::Interrupt),
            "the request is consumed once it has been journaled"
        );
        let worktrees = crate::list_worktrees(&repo.path).expect("list_worktrees");
        assert_eq!(worktrees.len(), 1, "no worktree may be left: {worktrees:?}");
    }

    /// A `verify_command` that passes, having first dropped `marker` into
    /// `project`'s control directory: a request that arrives while the
    /// completion gates run, after which the runner has no phase boundary
    /// left in that task to notice it at.
    fn verify_that_sends(project: &Project, marker: &str) -> Vec<String> {
        let dir = project.state_dir.join("control");
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "mkdir -p \"$1\" && touch \"$1/$2\"".to_string(),
            "_".to_string(),
            dir.display().to_string(),
            marker.to_string(),
        ]
    }

    #[test]
    fn an_interrupt_that_lands_after_the_last_phase_boundary_stops_the_queue_before_the_next_task()
    {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2)];
        // Never fires: the request comes from task 1's completion gate.
        let mut runner = signalled_runner(&project, &tasks, Request::Interrupt, 99);
        runner.config.verify_command = Some(verify_that_sends(&project, "interrupt"));
        runner.profile = profile_from(&runner.config).expect("profile_from");

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Interrupted);
        assert_eq!(state_of(&project, &tasks[0]), TaskState::Done);
        assert_eq!(
            state_of(&project, &tasks[1]),
            TaskState::Queued,
            "the next task must not have started"
        );
        assert!(!pending(&project, Request::Interrupt));
    }

    #[test]
    fn a_cancel_sent_during_a_task_cancels_it_and_the_queue_proceeds_to_its_successor() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1), sample_task(2)];
        // Task 1's steps are consumed by its probe and `Implement`; the
        // cancel stops it after that phase, and task 2 then needs its own.
        let mut runner = signalled_runner(&project, &tasks, Request::Cancel(tasks[0].id), 2);

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, &tasks[0]), TaskState::Cancelled);
        assert_eq!(state_of(&project, &tasks[1]), TaskState::Done);
        assert!(!pending(&project, Request::Cancel(tasks[0].id)));
        let journal = Journal::open_for(&project).expect("journal");
        let events = journal.events_for(tasks[0].id).expect("events");
        assert!(
            matches!(
                &events.last().expect("an event").kind,
                EventKind::TaskCancelled { reason } if reason.contains("cancel")
            ),
            "the last event must be TaskCancelled, got {:?}",
            events.last()
        );
    }

    #[test]
    fn a_cancel_that_arrives_too_late_to_stop_a_task_does_not_outlive_it() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let tasks = vec![sample_task(1)];
        let mut runner = signalled_runner(&project, &tasks, Request::Pause, 99);
        runner.config.verify_command = Some(verify_that_sends(&project, "cancel-1"));
        runner.profile = profile_from(&runner.config).expect("profile_from");

        let outcome = runner.run_queue(&tasks, None).expect("run_queue");

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(state_of(&project, &tasks[0]), TaskState::Done);
        assert!(
            !pending(&project, Request::Cancel(tasks[0].id)),
            "a cancel for a finished task must not linger"
        );
    }

    #[test]
    fn an_interrupt_sent_during_remediation_starts_no_further_round() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let reports = [1, 2, 3, 4]
            .map(|attempt| success_step(&report_path(&project, task.id, AttemptId::new(attempt))));
        let [first, second, third, fourth] = reports;
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let provider = Signaller {
            inner: Dummy::new(Scenario {
                // A fourth invocation would find a step, so only the count
                // can show that none was made.
                steps: vec![probe_step(), first, second, third, fourth],
            }),
            project: project.clone(),
            request: Request::Interrupt,
            // The first remediation round's invocation.
            on_call: 3,
            calls: calls.clone(),
        };
        let mut config = runnable_config();
        config.verify_command = Some(vec!["false".to_string()]);
        config.max_remediation_attempts = 3;
        let mut runner = manual_runner(&project, config, Box::new(provider));

        let state = runner
            .run_task(&task)
            .expect("an interrupt is not a failure");

        assert!(
            matches!(
                state,
                TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    ..
                }
            ),
            "got {state:?}"
        );
        assert_eq!(
            calls.get(),
            3,
            "probe, the first attempt and one remediation round; no more"
        );
    }

    #[test]
    fn an_interrupt_sent_during_remediation_parks_the_task_instead_of_failing_it() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let first = report_path(&project, task.id, AttemptId::new(1));
        let retry = report_path(&project, task.id, AttemptId::new(2));
        let scenario = Scenario {
            steps: vec![probe_step(), success_step(&first), success_step(&retry)],
        };
        let provider = Signaller {
            inner: Dummy::new(scenario),
            project: project.clone(),
            request: Request::Interrupt,
            // The remediation round's own invocation.
            on_call: 3,
            calls: std::rc::Rc::default(),
        };
        // The gate never passes, so remediation cannot succeed on its own.
        let mut config = runnable_config();
        config.verify_command = Some(vec!["false".to_string()]);
        let mut runner = manual_runner(&project, config, Box::new(provider));

        let state = runner
            .run_task(&task)
            .expect("an interrupt is not a failure");

        assert!(
            matches!(
                state,
                TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    ..
                }
            ),
            "got {state:?}"
        );
        assert!(!pending(&project, Request::Interrupt));
    }

    /// A [`Provider`] that hands every prompt it is given to an inner
    /// [`Dummy`] after keeping a copy, so a test can prove what a fresh
    /// session was actually seeded with.
    struct PromptRecorder {
        inner: Dummy,
        prompts: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl Provider for PromptRecorder {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
            self.prompts.borrow_mut().push(inv.prompt.clone());
            self.inner.invoke(inv, bus)
        }
    }

    /// A dummy step that succeeds, optionally printing `stdout` and writing
    /// `report` (path and content) as the agent would.
    fn dummy_step(stdout: Option<&str>, report: Option<(PathBuf, &str)>) -> Step {
        Step {
            on_task: None,
            on_attempt: None,
            outcome: StepOutcome::Success,
            stdout: stdout.map(str::to_string),
            exit_code: Some(0),
            delay_ms: None,
            files: report
                .into_iter()
                .map(|(path, content)| ScenarioFile {
                    path,
                    content: content.to_string(),
                })
                .collect(),
        }
    }

    /// A failing gate result whose captured stderr is `stderr`.
    fn red_gate(stderr: &str) -> GateResult {
        GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 1,
            stdout: String::new(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    /// Journals `task` as having run attempt 1 to a verification failure
    /// (its `gates` recorded) and then failed for good: the history a
    /// `Failed` task carries into `retry`.
    fn journal_failed_task(project: &Project, task: &Task, gates: Vec<GateResult>) {
        let mut journal = Journal::open_for(project).expect("open journal");
        let attempt = AttemptId::new(1);
        let mut history = vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt,
                protocol: "direct".to_string(),
                pid: 4242,
                base_sha: "base".to_string(),
            },
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        ];
        history.extend(
            gates
                .into_iter()
                .map(|result| EventKind::GateFinished { result }),
        );
        history.push(EventKind::VerifyFailed {
            attempt,
            class: FailureClass::VerificationFailure,
            detail: "the completion gates failed".to_string(),
        });
        history.push(EventKind::TaskFailed {
            class: FailureClass::VerificationFailure,
            detail: "the completion gates failed".to_string(),
        });
        for kind in &history {
            journal.append(Some(task.id), kind).expect("append history");
        }
    }

    /// Evidence for attempt 1 having failed, as `remediate` leaves it.
    fn write_failed_first_attempt(project: &Project, task: &Task) {
        let now = OffsetDateTime::now_utc();
        let record = AttemptRecord {
            id: AttemptId::new(1),
            task: task.id,
            started: now,
            ended: Some(now),
            model_configured: None,
            model_reported: None,
            session_id: Some("first-session".to_string()),
            exit_reason: "VerificationFailure: the completion gates failed".to_string(),
            gates: Vec::new(),
            usage: None,
            base_sha: "base".to_string(),
            candidate_sha: None,
        };
        write_evidence(project, &record, "").expect("write evidence");
    }

    fn event_kinds(project: &Project, task: &Task) -> Vec<&'static str> {
        Journal::open_for(project)
            .expect("open journal")
            .events_for(task.id)
            .expect("events_for")
            .iter()
            .map(|event| event.kind.discriminant())
            .collect()
    }

    #[test]
    fn retry_task_runs_a_fresh_session_seeded_with_the_failure_bundle_and_reaches_done() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        journal_failed_task(&project, &task, vec![red_gate("lint exploded on line 7")]);
        write_failed_first_attempt(&project, &task);
        let retry_report = report_path(&project, task.id, AttemptId::new(2));

        let prompts = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let provider = PromptRecorder {
            inner: Dummy::new(Scenario {
                steps: vec![
                    dummy_step(None, None),
                    dummy_step(
                        Some("repaired\n"),
                        Some((
                            retry_report.clone(),
                            "KTASK_RESULT: DONE\nSummary: fixed.\n",
                        )),
                    ),
                ],
            }),
            prompts: prompts.clone(),
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(provider));

        let state = runner
            .retry_task(&task)
            .expect("a failed task can be retried");

        assert_eq!(state, TaskState::Done);
        let prompts = prompts.borrow();
        assert_eq!(prompts.len(), 2, "one preflight probe, one retry session");
        let prompt = &prompts[1];
        for expected in [
            "Classification: VerificationFailure",
            "lint exploded on line 7",
            "Prior attempt 1: exit_reason=VerificationFailure",
            &retry_report.display().to_string(),
        ] {
            assert!(
                prompt.contains(expected),
                "the retry prompt must carry {expected:?}, got:\n{prompt}"
            );
        }
        assert!(
            !prompt.contains("first-session"),
            "a retry never carries the failed attempt's session forward, got:\n{prompt}"
        );

        let kinds = event_kinds(&project, &task);
        let after_failure = &kinds[kinds.iter().position(|k| *k == "TaskFailed").unwrap() + 1..];
        assert_eq!(
            after_failure,
            [
                "RetryStarted",
                "PhaseEntered",
                "AgentOutput",
                "AttemptFinished",
                "PhaseEntered",
                "VerifyPassed",
                "PublishStarted",
                "PublishVerified",
                "TaskDone",
            ]
        );
        let records = read_evidence(&project, task.id).expect("read_evidence");
        assert_eq!(records.len(), 2, "attempt 1 stays; the retry is attempt 2");
        assert_eq!(records[1].id, AttemptId::new(2));
        assert_eq!(records[1].exit_reason, "remediated");
        assert!(records[1].candidate_sha.is_some());
        assert_eq!(records[1].session_id, None);
        assert_eq!(
            crate::list_worktrees(&repo.path)
                .expect("list_worktrees")
                .len(),
            1,
            "the retry's worktree must be removed"
        );
    }

    #[test]
    fn an_interrupt_sent_during_a_retry_parks_the_task_instead_of_failing_it_again() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        journal_failed_task(&project, &task, vec![red_gate("lint exploded")]);
        write_failed_first_attempt(&project, &task);
        let provider = Signaller {
            inner: Dummy::new(Scenario {
                steps: vec![
                    probe_step(),
                    // No report: whatever ended the agent early left none.
                    dummy_step(None, None),
                ],
            }),
            project: project.clone(),
            request: Request::Interrupt,
            on_call: 2,
            calls: std::rc::Rc::default(),
        };
        let mut runner = manual_runner(&project, runnable_config(), Box::new(provider));

        let state = runner
            .retry_task(&task)
            .expect("an interrupt is no failure");

        assert!(
            matches!(
                state,
                TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    ..
                }
            ),
            "got {state:?}"
        );
        let kinds = event_kinds(&project, &task);
        assert_eq!(kinds.last(), Some(&"Interrupted"), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|kind| **kind == "TaskFailed").count(),
            1,
            "only the original failure is recorded: {kinds:?}"
        );
        assert!(!pending(&project, Request::Interrupt));
    }

    #[test]
    fn retry_task_that_fails_again_journals_task_failed_so_it_can_be_retried_once_more() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        journal_failed_task(&project, &task, Vec::new());
        // No report is written: the agent "ran" and produced nothing.
        let provider = Dummy::new(Scenario {
            steps: vec![dummy_step(None, None), dummy_step(Some("gave up\n"), None)],
        });
        let mut runner = manual_runner(&project, runnable_config(), Box::new(provider));

        let err = runner
            .retry_task(&task)
            .expect_err("a retry whose agent wrote no report fails");

        assert!(matches!(err, Error::Report { .. }), "got {err:?}");
        let kinds = event_kinds(&project, &task);
        assert_eq!(
            kinds.last(),
            Some(&"TaskFailed"),
            "the failure must be journaled, not left mid-attempt: {kinds:?}"
        );
        let state = journaled_state(&project, task.id).expect("journaled_state");
        assert!(
            matches!(
                state,
                TaskState::Failed {
                    class: FailureClass::AgentFailure,
                    ..
                }
            ),
            "got {state:?}"
        );
        let records = read_evidence(&project, task.id).expect("read_evidence");
        assert_eq!(records.len(), 1);
        assert!(records[0].exit_reason.contains("AgentFailure"));
        assert_eq!(records[0].candidate_sha, None);
    }

    #[test]
    fn retry_task_refuses_a_task_that_is_not_failed_and_journals_nothing() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        let mut runner = manual_runner(
            &project,
            runnable_config(),
            Box::new(Dummy::new(Scenario { steps: Vec::new() })),
        );

        let err = runner
            .retry_task(&task)
            .expect_err("a queued task is not failed");

        assert!(
            matches!(&err, Error::InvalidTransition { from, .. } if from == "Queued"),
            "got {err:?}"
        );
        assert_eq!(event_kinds(&project, &task), Vec::<&str>::new());
    }

    #[test]
    fn retry_task_stays_failed_when_preflight_still_fails() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let task = sample_task(1);
        journal_failed_task(&project, &task, Vec::new());
        let before = event_kinds(&project, &task);
        let mut config = runnable_config();
        config.baseline_command = Some(vec!["false".to_string()]);
        let mut runner = manual_runner(
            &project,
            config,
            Box::new(Dummy::new(Scenario { steps: Vec::new() })),
        );

        let err = runner
            .retry_task(&task)
            .expect_err("a red baseline still blocks the retry");

        assert!(matches!(err, Error::Preflight { .. }), "got {err:?}");
        assert_eq!(
            event_kinds(&project, &task),
            before,
            "a retry that never started records nothing against the task"
        );
    }

    #[test]
    fn record_failure_leaves_verifying_through_verify_failed_and_leaves_remediating_directly() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_for(&repo.path, state_dir.path());
        let attempt = AttemptId::new(1);
        let mut runner = manual_runner(
            &project,
            runnable_config(),
            Box::new(Dummy::new(Scenario { steps: Vec::new() })),
        );

        // Task 1 failed while `Verifying` (a commit that could not be made):
        // `Verifying` has no `TaskFailed` edge, so `VerifyFailed` comes first.
        let verifying = sample_task(1);
        {
            let mut journal = Journal::open_for(&project).expect("open journal");
            for kind in [
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "base".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt,
                    protocol: "direct".to_string(),
                    pid: 4242,
                    base_sha: "base".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt,
                    phase: Phase::Verify,
                },
            ] {
                journal.append(Some(verifying.id), &kind).expect("append");
            }
        }
        runner
            .record_failure(
                &verifying,
                attempt,
                FailureClass::GitConflict,
                "could not commit".to_string(),
            )
            .expect("record_failure from Verifying");
        let kinds = event_kinds(&project, &verifying);
        assert_eq!(&kinds[kinds.len() - 2..], ["VerifyFailed", "TaskFailed"]);
        assert!(matches!(
            journaled_state(&project, verifying.id).expect("state"),
            TaskState::Failed {
                class: FailureClass::GitConflict,
                ..
            }
        ));

        // Task 2 is already `Remediating` (its gate failed and said so): only
        // `TaskFailed` is recorded, never a second `VerifyFailed`.
        let remediating = sample_task(2);
        {
            let mut journal = Journal::open_for(&project).expect("open journal");
            for kind in [
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "base".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt,
                    protocol: "direct".to_string(),
                    pid: 4242,
                    base_sha: "base".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt,
                    phase: Phase::Verify,
                },
                EventKind::VerifyFailed {
                    attempt,
                    class: FailureClass::VerificationFailure,
                    detail: "red".to_string(),
                },
            ] {
                journal.append(Some(remediating.id), &kind).expect("append");
            }
        }
        runner
            .record_failure(
                &remediating,
                attempt,
                FailureClass::VerificationFailure,
                "still red".to_string(),
            )
            .expect("record_failure from Remediating");
        let kinds = event_kinds(&project, &remediating);
        assert_eq!(&kinds[kinds.len() - 2..], ["VerifyFailed", "TaskFailed"]);
        assert_eq!(kinds.iter().filter(|k| **k == "VerifyFailed").count(), 1);
    }
}

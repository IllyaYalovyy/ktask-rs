//! The runner: what one run is made of, gathered once, and the attempt it opens.
//!
//! A run needs five things and no more: the project it works, the configuration
//! that project was registered with, the gates that configuration configures, the
//! adapter the work is handed to, and the recorder every transition comes through.
//! [`Runner`] holds exactly those five, builds them from nothing but a
//! [`Project`], and refuses before anything is written when the settings describe a
//! run this build cannot have — a profile with no mandatory gate, an adapter no
//! adapter answers to. The reason it takes no configuration, no profile and no
//! adapter as arguments is that a run assembled from what its caller happened to
//! carry is a run whose gates and journal somebody else chose.
//!
//! Its seven jobs so far are [`Runner::prepare`], [`Runner::begin_attempt`],
//! [`Runner::run_phase`], [`Runner::gate_phase`], [`Runner::verify_and_publish`] and
//! [`Runner::run_task`] and the round trip an attempt's report makes. The first takes
//! a queued task as far as the ground it stands on; the second is the transition that
//! spends a token, and three things about it are not free to change:
//!
//! - The [`crate::EventKind::AttemptStarted`] row is appended before the attempt's
//!   evidence is filed, because VISION.md §3's third invariant makes the journal the
//!   account of what happened and evidence an after-effect of it.
//! - The base the row names is the one [`crate::EventKind::PreflightPassed`]
//!   recorded for that task, not wherever `HEAD` happens to stand. An attempt based
//!   on a tree nothing proved is what the `preflight` state exists to prevent, so a
//!   task with no recorded base is refused rather than based on a guess
//!   (ADR-0083).
//! - The evidence directory is written at the start rather than only at the end,
//!   because the run that dies mid-attempt is the case recovery is built for, and
//!   it has to find something to read.
//!
//! [`Runner::prepare`] is the step in front of that one, and its shape carries the
//! decisions behind it: the verdict of [`preflight`] is journaled here rather than
//! by the checks that earned it, the repository lock is taken after that verdict
//! and held by the [`Prepared`] value the step hands back, and the task's checkout
//! is cut from the fetched tip rather than from wherever `HEAD` stands
//! (VISION.md §10, ADR-0043).
//!
//! The report round trip is the one part of a session's handling that is here
//! already, because both ends of it are promises the run makes rather than things
//! it observes: [`Runner::prepare_report`] makes the directory the prompt names
//! before a provider is started, and [`Runner::read_report`] is the reading done
//! after one exits. They stay steps of their own because a report the run cannot
//! locate is indistinguishable from an agent that wrote nothing, which is a failure
//! the run owns whatever the session did — and [`Runner::run_phase`] calls them
//! around a session rather than folding them into it.
//!
//! [`Runner::run_phase`] is the middle those two ends bracket: one phase of a task's
//! protocol, from the row that marks it to the check on what its session was allowed to
//! touch. Four things about it are not free to change:
//!
//! - [`crate::EventKind::PhaseEntered`] is appended before the session starts, so an
//!   interrupted run resolves to a phase that was entered and never finished rather
//!   than to one that never began (VISION.md §3's third invariant).
//! - [`crate::provider::check_model`] is asked before one word of the session's
//!   output is journaled, so an attempt that ran on a model nobody chose leaves no
//!   rows attributed to a run the run has already refused (VISION.md §12).
//! - What the session touched is measured by [`protocol::check_scope`] over
//!   [`git::changed_paths`] from [`Prepared::base_sha`] — tracked edits and files
//!   nobody staged, because an untracked file is what a session that ignored its
//!   scope leaves. A violation is [`Error::Policy`] naming every path, and it
//!   outranks whatever the agent claimed.
//! - The agent's own account comes back as [`PhaseOutcome`], class included: a
//!   session that left no report is a classified failure and never an assumed
//!   success, and the class is the one [`crate::ReportClaim::Missing`] reported
//!   rather than one re-derived from an error (ADR-0057, ADR-0087).
//!
//! [`Runner::gate_phase`] is the mechanical half of the same step. VISION.md §9 makes a
//! red phase's failing test and a green phase's passing one the runner's own findings
//! rather than the agent's claims, so the gate the phase declares is run here, the
//! verdict is read out of the two test summaries the phase ran between, and the command,
//! the output and the tree hash are filed beside the attempt as its evidence. What is
//! still not here is the state a verdict moves and the remediation a refusal earns:
//! this module runs, records and refuses, and starts no state transition of its own.
//!
//! [`Runner::verify_and_publish`] is where the run stops merely measuring. VISION.md
//! §10 makes publication the one place a task's whole story has to add up by itself:
//! the work is committed, the completion set runs against that commit, and only a
//! candidate the remote is read back holding earns
//! [`crate::TaskState::PublishedVerified`]. Two of its refusals are decisions rather
//! than measurements — a tree holding uncommitted work, and a replay that stops on
//! conflicting content — and both end the attempt in the journal rather than handing
//! the work back to an agent to sort out.
//!
//! [`Runner::run_task`] is the order those six go in, and the one job that measures
//! nothing of its own. What it owns is what a caller assembling the steps by hand
//! would be free to assemble wrongly: which phases of a protocol no session is
//! started for, what a phase that decides by a difference is measured against before
//! it begins, which answers from a phase's own session mean its gate has nothing to
//! prove, and what the run gives back — its checkout and its repository lock — on
//! every way out of it, the refusal in particular.
//!
//! # Preflight: the checks that prove the world is sane before a token is spent
//!
//! VISION.md §6 puts one state between a queued task and a running one, and it
//! gives that state exactly one job — *"proves the world is sane before spending
//! tokens: clean fetched mainline, green `baseline_command`, provider available,
//! disk space, lock acquired"*. Five checks, one call: [`preflight`]. Nothing an
//! attempt would depend on is assumed by this module; every one of the five is
//! asked of the machine, the repository, the configuration and the adapter, and
//! every answer is written down with the finding it belongs to.
//!
//! # Why a refusal is a report and not an error
//!
//! `Result<PreflightReport>` carries the *verdict* in the `Ok`: a check that
//! refused produced an answer, and the answer is what the run acts on. The
//! taxonomy in VISION.md §7 is read per class — `provider_configuration` and
//! `needs_input` pause for a human, `git_conflict` and `verification_failure`
//! may be remediated — so the class of a refusal has to arrive as data. It
//! cannot arrive through [`crate::Error`] instead: ADR-0057 records that
//! [`crate::classify()`] reads an error of the run's, not a check's, and a
//! preflight refusal that reached the classifier as an [`crate::Error::Git`]
//! would be re-derived rather than reported. `Err` here therefore means the one
//! thing that is not a verdict: preflight could not ask a question at all — the
//! state directory is not there, the profile the configuration describes cannot
//! be built, a journal row was refused.
//!
//! # The order the checks run in, and why it stops at the first refusal
//!
//! The point of the exercise is that nothing expensive happens until everything
//! cheap has agreed, so the checks run cheapest first: the adapter and the free
//! disk are answered in one instruction or two, the mainline needs one network
//! round trip, the baseline gate can run for minutes, and the lock is asked last
//! because it is the check whose answer is stale the moment anyone else takes
//! the lock. The first refusal ends the run of checks: a full disk or a held
//! lock is answered without spending a gate's minutes behind it, and a report
//! carries every check that actually ran rather than a set of answers no check
//! gave.
//!
//! # What is journaled, and by whom
//!
//! VISION.md §3's third invariant — every transition persisted before its side
//! effect — is about the run, and the run owns the recorder. This module does
//! not have one: [`preflight`]'s signature is the project, the configuration and
//! the provider, so it opens the project's own journal to carry what it can
//! journal, which is the [`crate::GateKind::Baseline`] gate's pair of rows.
//! Those two are the evidence of the one check that runs a command, and
//! `docs/DESIGN.md` admits no entry for a check's result other than a
//! [`crate::EventKind::GateFinished`] carrying a [`crate::GateResult`].
//!
//! The verdict row is not written here, and that is not an oversight: the
//! catalog's `PreflightPassed` and `PreflightFailed` are the run's record of
//! having *started* the checks, and the caller that journaled `PreflightStarted`
//! is the one that must journal the answer — otherwise the same decision is
//! journaled twice by two connections, and `PreflightFailed` is the event that
//! ends a task. [`PreflightReport::event`] hands back exactly the row to write,
//! with the whole report's evidence in its `detail`, so a refusal that reaches
//! the journal is readable from the journal alone.
//!
//! # What preflight deliberately does not do
//!
//! - The provider check starts no session. "Provider available" is answered from what a
//!   `&dyn Provider` can say without being invoked; what that buys, and what it
//!   cannot, is recorded with `check_provider` below and in ADR-0082. It is a gap
//!   in the trait rather than a check that was skipped.
//! - It does not create a worktree, take the lock for the run, or hold anything.
//!   The lock check takes the lock and gives it back; the caller acquires the one
//!   its attempt runs under.
//! - It does not compare the checkout it was handed with the tip it fetched. A
//!   checkout standing behind `origin` is ordinary, and the work is based on the
//!   fetched tip rather than on `HEAD`, so drift is settled by publication
//!   (ADR-0046) instead of being refused here.
//!
//! [`RunOutcome`] is the module's one answer type rather than one of its jobs:
//! a queue-draining command hands it back, and it says what happened in the words
//! `docs/CONTRACT.md` §1 documents — with no number in it, because the number a
//! process leaves behind belongs to the CLI that runs one (T105 maps them).

use std::fmt;
use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::statvfs;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use crate::config;
use crate::git;
use crate::lock;
use crate::protocol;
use crate::provider;
use crate::redact::redact_json;
use crate::remediate::{Bounds, Breaker, BreakerState, Decision, RecoveryReport};
use crate::{
    AttemptId, AttemptRecord, Bus, Capabilities, Config, Error, EventKind, FailureClass, Gate,
    GateKind, GateResult, Invocation, Journal, Outcome, PauseReason, Phase, PhaseSpec, Profile,
    Project, Provider, Recorder, ReportClaim, ReportResult, Result, Stream, Subscription, Task,
    TaskId, TaskState, TestSummary, Usage, WaitPlan, apply, bundle, check_no_policy_edit, classify,
    decision_event, evidence_dir, file_report, limit_message, parse_cargo, parse_reset,
    policy_edit_event, profile_from, read_evidence, run_completion_set, run_gate, should_continue,
    signature, trip_event, wait_plan, write_evidence,
};
use crate::{context, queue};

/// The parts one run is made of, gathered once from the project it works.
///
/// Five fields, and each of them answers a question a run would otherwise have to
/// ask again — and could ask differently — at every transition:
///
/// - `project`: which repository, and where its journal, its lock and its
///   evidence live.
/// - `config`: what the project's own settings document, the machine's document
///   and the environment said, with the layers already resolved.
/// - `profile`: the gates that configuration configures, in the order a run
///   executes them, with the mandatory gate already proved present.
/// - `recorder`: the one door a transition comes through, so nothing can be
///   published without being journaled and nothing journaled without being told
///   (ADR-0016).
/// - `provider`: the adapter the configured word named, built once so no two
///   attempts of one run can be handed to different CLIs.
///
/// A run is *not* a piece of state: it holds no phase, no task and no attempt,
/// because those are the projection of the journal and the journal is the source
/// of truth (VISION.md §3). What it holds is everything the journal needs a caller
/// to already have settled.
pub struct Runner {
    /// The registered repository the run works, and the state directory its
    /// durable data is written into.
    project: Project,
    /// The configuration this project runs on, read through the layers
    /// [`config::load_for`] resolves.
    config: Config,
    /// The gates [`crate::profile_from`] built from that configuration, already
    /// validated, in the order a run runs them.
    profile: Profile,
    /// The journal-and-bus pair every transition of this run comes through.
    recorder: Recorder,
    /// The adapter [`provider::build`] made from the configured word.
    provider: Box<dyn Provider>,
}

impl Runner {
    /// Open the run one registered project is configured to have.
    ///
    /// A project is the only argument, which is the point: the settings, the
    /// gates, the adapter and the journal are all consequences of it, so a caller
    /// cannot assemble a run from a configuration it did not read. The four are
    /// resolved in the order their refusals get cheaper, and nothing at all is
    /// written before all four have agreed:
    ///
    /// 1. [`config::load_for`] reads what this project is configured to be.
    /// 2. [`profile_from`] builds the gates from it, refusing a project with no
    ///    complete local suite — VISION.md §8's mandatory gate.
    /// 3. [`provider::build`] makes the adapter the configured word names.
    /// 4. [`Journal::open_for`] opens the journal, and [`Recorder::with_bus`]
    ///    gives it the bus whose rings are the configured `output_ring_lines`,
    ///    because a run's screens are sized by its own settings and not by the
    ///    compiled-in default.
    ///
    /// A refusal at 1, 2 or 3 therefore leaves no journal behind: a run that never
    /// began has no transitions to record, and an empty database file is the
    /// impression that somebody else had started work here.
    ///
    /// The live view a frontend follows comes from [`Runner::subscribe`] rather
    /// than from a sixth field: a run that held its own view would be a subscriber
    /// that never reads, whose ring overflows and counts a loss for every event of
    /// its own run.
    ///
    /// # Errors
    ///
    /// As the four calls above: [`Error::Config`] for a key whose value cannot be
    /// read, for a missing `verify_command`, and for an adapter word this build has
    /// no adapter for; [`Error::Io`] for a settings document that is there and
    /// cannot be read; [`Error::Database`] for a project whose state directory is
    /// not there — registration owns that directory, so this refuses rather than
    /// conjuring one — and [`Error::Corrupt`] on an unreadable journal.
    pub fn new(project: Project) -> Result<Self> {
        let config = config::load_for(&project)?;
        let profile = profile_from(&config)?;
        let provider = provider::build(&config)?;
        let journal = Journal::open_for(&project)?;
        Ok(Self {
            recorder: Recorder::with_bus(journal, Bus::with_capacity(config.output_ring_lines)),
            project,
            config,
            profile,
            provider,
        })
    }

    /// A view of this run, for whoever is watching it: the TUI's event stream, the
    /// CLI's `--follow` output, the log's own reader.
    ///
    /// What was recorded before the call is not replayed — the journal is where
    /// the past is read — and a view that comes and goes costs one ring, so a
    /// screen that closes mid-run neither loses the run nor blocks it.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.recorder.subscribe()
    }

    /// Take `task` from the queue to the point where an agent could start.
    ///
    /// VISION.md §6 puts exactly one state between `queued` and `running` and gives
    /// it one job — prove the world sane before a token is spent — and this is the
    /// step that walks it: the five checks are asked, the verdict they earned is
    /// journaled, the repository lock is taken, and the task's own checkout is cut
    /// from the commit the fetch brought back. What comes back is [`Prepared`],
    /// which holds what the next step needs and starts nothing of its own.
    ///
    /// **The start is journaled before the checks are asked.** VISION.md §3's third
    /// invariant makes the journal the account of what happened rather than a
    /// summary of what finished, and a preflight that dies halfway — a baseline
    /// command that outlived its budget, a machine that lost power — has to leave
    /// [`crate::EventKind::PreflightStarted`] behind. The alternative is a task that
    /// still reads as `queued` while its own journal holds a gate's rows, which is
    /// the ambiguity recovery exists to make impossible.
    ///
    /// **The verdict is written here, not by [`preflight`].** ADR-0082 hands the
    /// verdict row to whoever journaled the start, and
    /// [`crate::EventKind::PreflightFailed`] is the row that ends a task: a second
    /// writer would append the same decision twice. The row is
    /// [`PreflightReport::event()`] unchanged, so the class the refusing check named
    /// is the class the journal holds. Nothing here re-derives it, and
    /// [`crate::classify()`] is never asked to guess what a check already said
    /// (ADR-0057).
    ///
    /// **The lock is taken after the verdict, with no wait, and then held.** After
    /// the verdict because a refusal has no business holding a lock nobody is about
    /// to need: VISION.md §6 lists the lock among the five checks, and that check's
    /// own answer is "taken and given back" (ADR-0082), so a step that kept it past
    /// a refusal would report the machine busy on behalf of a task it had just
    /// refused. With no wait because nothing is configured to wait for —
    /// [`crate::Config`] holds no lock timeout — and because waiting is recovery's
    /// decision, made from the class this step has already journaled. Held, rather
    /// than taken and given back, because VISION.md §10's step 5 publishes under
    /// this lock: what makes "the checkout was cut from the fetched tip" still true
    /// when the candidate is pushed is the lock.
    ///
    /// **The checkout is cut from the base the report named, never from `HEAD`.**
    /// [`crate::git::create_worktree`] is handed the report's own base — the tip
    /// `<remote>/<branch>` had when the fetch moved it — because a task started from
    /// wherever the user's checkout happened to stand would be verified against a
    /// commit nobody fetched (VISION.md §10's step 2). The name is one per task, so
    /// the second time this step runs for a task it hands back the checkout that
    /// stopped — which is what VISION.md §7 requires a remediation to find.
    ///
    /// # Errors
    ///
    /// As [`preflight`] for anything that stopped the checks from being asked at all:
    /// [`Error::Database`] for a state directory that is not there and for a gate row
    /// that was refused, [`Error::Config`] for a configuration that describes no
    /// runnable profile. [`Error::NotFound`] when the checks *did* answer and refused:
    /// the prepared task this signature promises does not exist, exactly as
    /// `recorded_base` refuses a task no `PreflightPassed` ever gave a base, and the
    /// message carries the check that refused, the class it named, and every line of
    /// evidence the report holds. [`Error::NotFound`], [`Error::Io`] and
    /// [`Error::Policy`] as [`crate::lock::acquire`], and [`Error::Git`] and
    /// [`Error::Policy`] as [`crate::git::create_worktree`]. Those two come after the
    /// verdict has been journaled, which is the order VISION.md §3 asks for: whatever
    /// the side effect did next, the row is there for recovery to read.
    pub fn prepare(&mut self, task: &Task) -> Result<Prepared> {
        self.recorder
            .record(Some(task.id), EventKind::PreflightStarted)?;
        let report = preflight(&self.project, &self.config, self.provider.as_ref())?;
        self.recorder.record(Some(task.id), report.event())?;
        if let Some(&CheckOutcome::Refused { check, class, .. }) = report.refusal() {
            return Err(Error::NotFound {
                what: format!(
                    "task {id}'s checkout: preflight refused `{check}` as {class:?} — {evidence}",
                    id = task.id,
                    evidence = report.evidence()
                ),
            });
        }
        let base_sha = report.base_sha.clone();
        let held = lock::acquire(&self.project.state_dir, Duration::ZERO)?;
        let worktree =
            git::create_worktree(&self.project.root, &worktree_name(task.id), &base_sha)?;
        Ok(Prepared {
            worktree,
            base_sha,
            lock: held,
        })
    }

    /// Open one attempt of `task`, and return the id it was given.
    ///
    /// This is the door between `preflight` and `running`, and the last step that
    /// costs nothing. Three facts go into the row, none of them guessed at: the
    /// protocol [`protocol::for_task`] resolved from the task's own word, the
    /// project's default and `direct` in that order; the process id recovery would
    /// go looking for to tell a dead run from a live one; and the base
    /// `recorded_base` found in the journal rather than read off `HEAD`.
    /// The attempt number continues from the highest one already journaled for the
    /// task, so a supervisor that started again numbers a retry after the attempts
    /// it no longer remembers.
    ///
    /// The order of the two writes is the invariant, not an implementation detail:
    /// [`crate::EventKind::AttemptStarted`] is appended first, and only then is the
    /// [`AttemptRecord`] filed with [`write_evidence`]. A reader who arrives after
    /// a crash between the two finds a journaled attempt with no evidence — the
    /// shape recovery already treats as "this attempt never completed" — never an
    /// evidence directory for an attempt the journal has never heard of.
    ///
    /// The record filed at the start says what the attempt *is*: its task, its
    /// base, the model its configuration asked for, and that it is running as this
    /// pid. Everything about what it did — its session, its gates, its cost, the
    /// commit it produced, its end — is absent, and stays absent until the attempt
    /// has an answer to give. The context document beside it is empty: assembling
    /// the context is the runner's next step, not this one's.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] keyed `protocol`, `default_protocol` or `tdd_exception`
    /// when [`protocol::for_task`] cannot name a protocol this build runs;
    /// [`Error::NotFound`] when no [`crate::EventKind::PreflightPassed`] row gives
    /// this task a base; [`Error::Database`] and [`Error::Serde`] as
    /// [`Recorder::record`]; [`Error::Io`], [`Error::Policy`] or [`Error::Serde`]
    /// as [`write_evidence`]. A refusal before the append writes nothing at all.
    pub fn begin_attempt(&mut self, task: &Task) -> Result<AttemptId> {
        let protocol = protocol::for_task(task, &self.config)?;
        let journal = Journal::open_for(&self.project)?;
        let (base_sha, last) = recorded_base(&journal, task.id)?;
        let attempt = AttemptId::new(last + 1);
        let pid = std::process::id();

        self.recorder.record(
            Some(task.id),
            EventKind::AttemptStarted {
                attempt,
                protocol: protocol.name.to_owned(),
                pid,
                base_sha: base_sha.clone(),
            },
        )?;
        write_evidence(
            &self.project,
            &self.opened(attempt, task.id, pid, &base_sha),
            CONTEXT_AT_START,
        )?;
        Ok(attempt)
    }

    /// Make the directory an attempt's report will be written into, and hand back
    /// the file inside it that the report is.
    ///
    /// Called after the prompt is assembled and before the provider is started,
    /// because the prompt names [`crate::report_path`]'s path as where the report
    /// goes: a session told to write `<dir>/agent-report.md` into a directory that
    /// was never made meets an error, and the run cannot then tell an agent that
    /// refused to report from one that could not. Making the directory is the only
    /// way the promise in the prompt is the run's rather than the agent's.
    ///
    /// The path returned is *the* path — the one the header of the prompt prints
    /// and the one [`Runner::read_report`] reads — so a caller cannot hand a
    /// provider a directory and read back a different one.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when this project has no state directory, and
    /// [`Error::Policy`] or [`Error::Io`] when a level of the evidence layout is
    /// occupied or the filesystem refused it. Nothing is journaled here, so a
    /// refusal leaves the attempt exactly as it stood.
    pub fn prepare_report(&self, task: TaskId, attempt: AttemptId) -> Result<PathBuf> {
        crate::attempt::ensure_evidence_dir(&self.project, task, attempt)?;
        Ok(crate::report::report_path(&self.project, task, attempt))
    }

    /// Read what an attempt said about itself, once its provider has exited.
    ///
    /// A claim, never a verdict: what comes back is what the agent wrote, and the
    /// gates, the push and the fetched remote outrank it (VISION.md §3's invariants
    /// 4 and 7). What it can do is refuse — an attempt that left no report is
    /// [`crate::ReportClaim::Missing`], carrying the class the run records and the
    /// path the prompt named, and nothing about it is assumed.
    ///
    /// # Errors
    ///
    /// As [`crate::read_report`]: [`Error::Io`] when the filesystem refused the
    /// read for a reason other than absence, and [`Error::Corrupt`] when a report
    /// is there and its header cannot be trusted. Both are refusals of the read,
    /// and neither is a claim of completion.
    pub fn read_report(&self, task: TaskId, attempt: AttemptId) -> Result<ReportClaim> {
        crate::report::read_report(&self.project, task, attempt)
    }

    /// Work one phase of `task`'s protocol: run its session, and check what the
    /// session was allowed to touch.
    ///
    /// VISION.md §9 makes a protocol a list of phases and gives each phase a write
    /// scope, and §3's third invariant makes the row that marks a step come before
    /// the side effect it describes. This is the one step where both can be held at
    /// once — the phase, the prompt handed over, the checkout the session wrote
    /// into, and the account it left — which is why the check on what it touched
    /// lives here rather than in whatever reads the answer afterwards.
    ///
    /// The order of what is written is the behaviour, not the shape of a function:
    ///
    /// 1. [`crate::EventKind::PhaseEntered`] is appended before anything else, so a
    ///    run that dies during a session is read as a phase that was entered and
    ///    never finished — a state recovery knows how to resolve — and never as a
    ///    phase that never started.
    /// 2. The prompt is assembled from the documents where they live:
    ///    [`crate::build_prompt`]'s text, built over this project's own template and
    ///    the context document beside it, and [`crate::collect_adrs`] the decisions
    ///    below the repository's `docs/adr`. A session started without the decisions
    ///    on record would re-decide something already settled, and look like the
    ///    supervisor had forgotten — so a prompt that cannot be read refuses the
    ///    phase before a token is spent. The queue length the header prints is
    ///    [`queue::load`]'s,
    ///    read here rather than passed in: it is a fact about the journal, and a
    ///    caller that supplied it could supply a different one from the one the
    ///    queue holds.
    /// 3. The report's directory is made before the provider starts, because the
    ///    header names the file inside it (ADR-0085).
    /// 4. The session runs in `prep.worktree` — the task's own checkout — and never
    ///    in the repository the run supervises (VISION.md §10). [`Invocation`] holds
    ///    the prompt, the configured model id unchanged, and that directory: the
    ///    three things an adapter is allowed to know.
    /// 5. [`provider::check_model`] is asked *before* anything the session said is
    ///    written down. An attempt that ran on a model nobody chose is refused
    ///    rather than recorded (VISION.md §12), and refusing it first is what keeps
    ///    a refusal from leaving a session's output behind under an attribution the
    ///    run has already rejected.
    /// 6. One [`crate::EventKind::AgentOutput`] row per line the session printed —
    ///    stdout first, then stderr, because the difference between the two is
    ///    unrecoverable once merged and is what a classification reads first — and
    ///    then [`crate::EventKind::AttemptFinished`] with the four things only that
    ///    session knew. Nothing is filled in where the session said nothing
    ///    (ADR-0049, ADR-0057), and the configured model id is never copied into the
    ///    field that means *what the session reported*.
    /// 7. The report is read back as [`Runner::read_report`] reads it, and only then
    ///    is the checkout compared against the phase's scope with
    ///    [`protocol::check_scope`] over [`git::changed_paths`] from
    ///    `prep.base_sha` — tracked edits and files nobody staged, because an
    ///    untracked file is exactly what a session that ignored its scope leaves.
    ///
    /// # Why a scope violation is an error and a missing report is not
    ///
    /// A violation is the run's own check refusing, so it arrives as
    /// [`Error::Policy`] naming every path that broke the rule — the shape
    /// [`crate::classify()`] answers with `policy_failure`, the class VISION.md §7
    /// refuses to retry. A missing report is the *agent's* failure to account for
    /// itself, which [`crate::ReportClaim::Missing`] already carries as a class and
    /// a path: it comes back as [`PhaseOutcome::Unreported`] so the recovery policy
    /// reads the class it was given instead of one re-derived from an error
    /// (ADR-0057). Both arrived after the session's rows were written, because what
    /// a session did is a fact whoever ends it.
    ///
    /// The order of the last two rules is deliberate: a phase that wrote outside its
    /// scope *and* stayed silent is refused for the write. The scope is the rule the
    /// phase exists to hold, and an agent's silence beside a broken rule is the
    /// smaller finding — which is also why the check runs whatever the claim said,
    /// including a claim of `DONE`.
    ///
    /// # What this step does not do
    ///
    /// It moves no state: no [`crate::apply`], so a task still reads as the journal
    /// says it does, which is the phase's gate's work and the task after this one's
    /// (ADR-0086 is why a session's end is not it either). It runs no gate, and
    /// publishes nothing. It re-files no evidence: `begin_attempt` filed the
    /// attempt's `context.md`, and one attempt writes one record. And it hands the
    /// adapter no live view — see ADR-0087 for why the session is invoked unwatched
    /// and what that costs until a later task gives the recorder a bus to lend.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Database`] and [`crate::Error::Serde`] as [`Recorder::record`];
    /// the refusals of [`crate::build_prompt`] and [`crate::collect_adrs`] —
    /// [`crate::Error::Policy`] for a prompt library or a decision archive in the
    /// wrong shape, [`crate::Error::Config`] for a home neither environment nor
    /// configuration names — all before the session starts;
    /// [`crate::Error::Provider`] as [`Provider::invoke`] gives it, for a CLI that
    /// never ran; [`crate::Error::Config`] keyed `model` for the mismatch
    /// [`provider::check_model`] refuses; [`crate::Error::Corrupt`] as
    /// [`Runner::read_report`] gives it for a report whose header cannot be trusted;
    /// [`crate::Error::Git`] as [`git::changed_paths`] gives it; and
    /// [`crate::Error::Policy`] for the scope itself, naming every offending path.
    /// A phase refused at 1 or 2 has journaled its entry and nothing else; one
    /// refused at 5 has journaled nothing but that entry, and one refused at 7 has
    /// journaled the whole session.
    pub fn run_phase(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        spec: &PhaseSpec,
    ) -> Result<PhaseOutcome> {
        self.run_phase_with(&crate::paths::process_env, prep, task, attempt, spec)
    }

    /// [`Runner::run_phase`] with the environment the prompt's documents are read
    /// from supplied by the caller.
    ///
    /// Crate-visible rather than private to this module for the reason
    /// [`crate::paths::state_root_with`] is: `docs/DESIGN.md` Conventions keeps a test
    /// out of the process environment and out of the operator's real configuration,
    /// and the only way a phase's prompt can be read from a scratch directory is for
    /// the accessor to be handed to it. The same reason `context::ensure_defaults_with`
    /// is threaded out of its own module.
    fn run_phase_with(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        spec: &PhaseSpec,
    ) -> Result<PhaseOutcome> {
        let mut under = UnderAttempt::alone(prep, attempt);
        self.run_the_session(env, &mut under, task, spec)
    }

    /// The session one phase of a run is, and everything it leaves behind for the
    /// refusal it may earn.
    ///
    /// [`Runner::run_phase`] is the door a caller outside a run comes through; this is
    /// what a run itself calls. The difference is not the order of the rows — those are
    /// the same and for the same reasons — it is that a refusal has to be *classifiable*
    /// after the attempt stopped, and [`classify()`] reads an [`Outcome`] and a set of
    /// [`GateResult`] that nothing on disk holds. So the session's own answer is kept as
    /// soon as the provider gave it, before anything that could refuse the phase.
    ///
    /// Two things happen here that a first attempt never sees, and both are §7's rather
    /// than §9's:
    ///
    /// - The prompt of an attempt that *is* a remediation carries the bundle its
    ///   refusal produced, under a heading that says whose account of the refusal it
    ///   is. [`context::build_prompt`] is left alone (ADR-0075) — the documents it
    ///   assembles are the same ones a first attempt was handed — and the [`Invocation`]
    ///   carries no session id, because §7's repair is a new session told what the last
    ///   one was refused for, not the old one resumed.
    /// - Every attempt's changed paths are asked [`check_no_policy_edit`] before its
    ///   write scope is, including a first attempt's. An attempt that edited the rules
    ///   it is judged by is refused before anything is measured against the edited rule,
    ///   and the paths it touched travel out with the refusal so the row that ends the
    ///   task can name them.
    ///
    /// # Errors
    ///
    /// As [`Runner::run_phase`], plus [`Error::Policy`] naming every protected path the
    /// session touched.
    fn run_the_session(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        under: &mut UnderAttempt<'_>,
        task: &Task,
        spec: &PhaseSpec,
    ) -> Result<PhaseOutcome> {
        let attempt = under.attempt;
        let prep = under.ground;
        let work = Some(task.id);
        self.recorder.record(
            work,
            EventKind::PhaseEntered {
                attempt,
                phase: spec.phase,
            },
        )?;
        let mut prompt = context::build_prompt_with(
            env,
            &self.project,
            task,
            attempt,
            queue::load(&self.project)?.len(),
        )?;
        if let Some(repair) = under.repair {
            repair.seed(&mut prompt);
        }
        self.prepare_report(task.id, attempt)?;
        let outcome = self.provider.invoke(
            &Invocation {
                prompt,
                model: self.config.model.clone(),
                working_dir: prep.worktree.clone(),
            },
            None,
        )?;
        under.seen.session = Some(outcome.clone());
        provider::check_model(
            self.config.model.as_deref(),
            outcome.model_reported.as_deref(),
        )?;
        for (stream, printed) in [
            (Stream::Stdout, outcome.stdout.as_str()),
            (Stream::Stderr, outcome.stderr.as_str()),
        ] {
            for text in session_lines(printed) {
                self.recorder.record(
                    work,
                    EventKind::AgentOutput {
                        attempt,
                        stream,
                        text,
                    },
                )?;
            }
        }
        self.recorder.record(
            work,
            EventKind::AttemptFinished {
                attempt,
                exit_code: outcome.exit_code,
                usage: outcome.usage,
                session_id: outcome.session_id,
                model_reported: outcome.model_reported,
            },
        )?;
        let claim = self.read_report(task.id, attempt)?;
        if let ReportClaim::Claimed {
            result: ReportResult::NeedsInput,
            text,
            ..
        } = &claim
        {
            under.seen.asked = Some(text.clone());
        }
        let changed = git::changed_paths(&prep.worktree, &prep.base_sha)?;
        if let Err(refusal) = check_no_policy_edit(&changed) {
            if let Error::Policy { paths, .. } = &refusal {
                under.seen.policy_edit.clone_from(paths);
            }
            return Err(refusal);
        }
        protocol::check_scope(spec.write_scope, &changed, &self.config.test_globs)?;
        Ok(match claim {
            ReportClaim::Claimed {
                result: report,
                text,
                ..
            } => PhaseOutcome::Claimed {
                phase: spec.phase,
                attempt,
                report,
                text,
                changed,
            },
            ReportClaim::Missing {
                class,
                path,
                detail,
            } => PhaseOutcome::Unreported {
                phase: spec.phase,
                attempt,
                class,
                path,
                detail,
            },
        })
    }

    /// Run the gate one phase declares, and decide the phase from what it reported.
    ///
    /// VISION.md §9 gives the red and green phases to the runner rather than to the
    /// agent's account of itself: *"the runner executes `targeted_test_command` and
    /// confirms the expected new failure"*, and then the same for the fix that ends it.
    /// So this runs the command [`PhaseSpec::gate`] names, reads a [`TestSummary`] out
    /// of what it printed, and hands the pair of summaries — the one the phase started
    /// from and the one it just ran — to [`protocol::verify_red`] or
    /// [`protocol::verify_green`]. Five things about the order are not free to change:
    ///
    /// - A phase that decides itself by comparing two runs is refused before anything
    ///   is journaled when it was handed neither. Red with nothing to differ from is
    ///   not a red phase that failed; it is the step called wrongly, and the refusal
    ///   names the phase it could not decide.
    /// - [`crate::EventKind::GateStarted`] is appended before the command is spawned,
    ///   exactly as [`crate::run_completion_set`] and the preflight's baseline do it: a
    ///   gate that could not be started leaves its start with no finish after it, which
    ///   is the pair that reads as "this gate never completed" (ADR-0036).
    /// - The phase's verdict comes from the two summaries and not from the exit status,
    ///   and the exit status still has the last word. A red phase is *expected* to
    ///   refuse, and a green phase whose names all passed but whose command refused is
    ///   refused too: [`GateResult::passed`] is the gate's own verdict and the names are
    ///   an addition to it, never a substitute.
    /// - The verdict outranks the evidence filing, and the filing happens whatever the
    ///   verdict was. A refusal nobody can read is a refusal nobody can act on, so a
    ///   phase that proved nothing still files what it ran before it is refused.
    /// - A red phase whose task declared §9's exception to test-first runs no gate at
    ///   all: [`protocol::claim`] answers with the row that records the exception over
    ///   the paths the phase was allowed to write, and the phase hands on the summary it
    ///   started from.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the phase declares no gate, or names a comparison it
    /// was not given. [`Error::Config`] when the gate it declares is configured with no
    /// command. [`Error::Policy`] when a red phase claimed the exception over paths its
    /// write scope did not grant. [`Error::Gate`] when the command could not be started,
    /// wrote no test report, or refused to decide the phase. [`Error::NotFound`],
    /// [`Error::Io`] and [`Error::Serde`] when the evidence it files could not be
    /// filed, and [`Error::Database`] when one of its two rows was refused.
    pub fn gate_phase(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        spec: &PhaseSpec,
        before: Option<&TestSummary>,
    ) -> Result<TestSummary> {
        let mut under = UnderAttempt::alone(prep, attempt);
        self.gate_the_phase(&mut under, task, spec, before)
    }

    /// [`Runner::gate_phase`] for a phase a run is working, which is the case where
    /// the gate's answer has to outlive the step that ran it.
    ///
    /// The [`GateResult`] is kept whatever the verdict then was. §7's failure signature
    /// and the class a remediation is chosen by are both read from the gates that
    /// refused, and a phase refused by its gate is precisely the attempt that earns the
    /// repair — leaving the result behind on that path would classify the commonest
    /// failure from an empty list.
    fn gate_the_phase(
        &mut self,
        under: &mut UnderAttempt<'_>,
        task: &Task,
        spec: &PhaseSpec,
        before: Option<&TestSummary>,
    ) -> Result<TestSummary> {
        let Some(kind) = spec.gate else {
            return Err(no_gate_declared(spec.phase));
        };
        let way = comparison(spec.phase, before)?;
        if let Comparison::Red(prior) = way
            && let Some(skipped) = self.excused_red(under.ground, task, spec, prior)?
        {
            return Ok(skipped);
        }
        let gate = self.configured_gate(kind)?;
        let result = self.run_declared_gate(task.id, &gate, &under.ground.worktree)?;
        under.seen.gates.push(result.clone());
        let summary = test_report(&result).ok_or_else(|| no_test_report(&gate, &result))?;
        let verdict = phase_verdict(way, &gate, &result, &summary);
        let named = verdict.as_ref().cloned().unwrap_or_default();
        let filed = if spec.records_evidence {
            let run = PhaseRun {
                gate: &gate,
                result: &result,
                named: &named,
            };
            self.file_phase_evidence(under.ground, task.id, under.attempt, spec.phase, &run)
        } else {
            Ok(())
        };
        verdict?;
        filed?;
        Ok(summary)
    }

    /// Honour §9's exception to test-first, if this red phase's task claimed one.
    ///
    /// The exception is not a pardon for what was written — it is a statement that
    /// nothing *needing* a new test was written — so the claim is checked against
    /// the same [`crate::WriteScope`] the phase itself was granted before it
    /// excuses anything: [`protocol::claim`] refuses a documentation exception
    /// claimed over a production path with the same [`Error::Policy`] that refuses
    /// the phase, and the run never reaches the gate that would have been skipped.
    ///
    /// Nothing is journaled but the exception itself. A phase that ran no command has
    /// no gate pair to leave, and an invented one would be read as a check that passed.
    ///
    /// # Errors
    ///
    /// [`Error::Policy`] when a path the phase changed sits outside its write scope,
    /// [`Error::Config`] when the task's declaration names no exception category §9
    /// knows, and [`Error::Database`] when the row recording it was refused.
    fn excused_red(
        &mut self,
        prep: &Prepared,
        task: &Task,
        spec: &PhaseSpec,
        started_from: &TestSummary,
    ) -> Result<Option<TestSummary>> {
        let changed = git::changed_paths(&prep.worktree, &prep.base_sha)?;
        let Some(event) =
            protocol::claim(task, spec.write_scope, &changed, &self.config.test_globs)?
        else {
            return Ok(None);
        };
        self.recorder.record(Some(task.id), event)?;
        Ok(Some(started_from.clone()))
    }

    /// The gate one phase's declaration named, as this project configured it.
    ///
    /// A phase declares *which* check decides it; the configuration decides *what that
    /// check runs*, and a declaration with nothing behind it is refused rather than
    /// skipped — a phase that was never gated and a phase whose gate passed are the
    /// same answer to everything downstream, which is exactly why they must not be.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming the setting that would have configured it.
    fn configured_gate(&self, kind: GateKind) -> Result<Gate> {
        self.profile
            .get(kind)
            .cloned()
            .ok_or_else(|| Error::Config {
                key: gate_setting(kind),
                detail: format!(
                    "the phase's declaration names the {kind} gate, and no command is \
                 configured for it, so nothing decides the phase"
                ),
            })
    }

    /// Run one declared gate, journalling the pair that says it ran.
    ///
    /// [`crate::run_completion_set`] journals this pair for the completion gates and
    /// [`check_baseline`] for the preflight's; this is it for a phase's own gate, and
    /// the pair is written under the task the phase belongs to rather than under no
    /// task at all. A failures screen opens a task's rows, and a phase whose gate
    /// appears nowhere in them is indistinguishable from a phase that ran none.
    ///
    /// # Errors
    ///
    /// [`Error::Gate`] when the command could not be started at all, with the
    /// [`crate::EventKind::GateStarted`] row left by itself: a gate that never ran
    /// cannot answer for what it would have found.
    fn run_declared_gate(&mut self, work: TaskId, gate: &Gate, root: &Path) -> Result<GateResult> {
        self.recorder
            .record(Some(work), EventKind::GateStarted { gate: gate.kind })?;
        let result = run_gate(gate, root, None).map_err(|failure| Error::Gate {
            kind: gate.kind.to_string(),
            detail: format!(
                "the gate `{}` could not be started: {failure}",
                command_words(gate)
            ),
        })?;
        self.recorder.record(
            Some(work),
            EventKind::GateFinished {
                result: result.clone(),
            },
        )?;
        Ok(result)
    }

    /// File one phase's evidence beside the attempt whose phase it was.
    ///
    /// VISION.md §9 wants RED and GREEN evidence — *command, output, tree hash* —
    /// stored with the attempt, and ADR-0065 gave an attempt a file home beside the
    /// journal for what a row cannot carry. `phases/<phase>.jsonl` is that rule held
    /// open for a phase that may gate more than once: [`write_evidence`] refuses a
    /// second, different record for one attempt, which is right for the record and
    /// wrong for a red phase that was refused and tried again, and an append-only line
    /// per run says both attempts happened instead of leaving only the last.
    ///
    /// The whole line is redacted with the project's own `secret_patterns` before it
    /// reaches the disk, which is the gap ADR-0065 recorded for the attempt's other
    /// files: gate output is where a test binary prints what it was testing, and a
    /// secret that only ever appears in evidence still reached the tree it was filed in.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the project has no state directory, [`Error::Policy`]
    /// when a level of the layout is there and is not a directory, [`Error::Io`] when
    /// the filesystem refused the append, and [`Error::Serde`] when the record had no
    /// JSON spelling or no redaction could be built.
    fn file_phase_evidence(
        &self,
        prep: &Prepared,
        work: TaskId,
        attempt: AttemptId,
        phase: Phase,
        run: &PhaseRun<'_>,
    ) -> Result<()> {
        crate::attempt::ensure_evidence_dir(&self.project, work, attempt)?;
        let dir = evidence_dir(&self.project, work, attempt).join(PHASES_DIR);
        private_directory(&dir)?;
        let evidence = PhaseEvidence {
            phase: phase_word(phase),
            gate: run.gate.kind.as_str(),
            command: &run.gate.command,
            base_sha: &prep.base_sha,
            tree_sha: tree_hash(prep)?,
            names: run.named,
            passed: run.result.passed,
            exit_code: run.result.exit_code,
            timed_out: run.result.timed_out,
            stdout: &run.result.stdout,
            stderr: &run.result.stderr,
        };
        let line = redact_json(
            &serde_json::to_string(&evidence)?,
            &self.config.secret_patterns,
        )?;
        let path = dir.join(format!("{}{JSONL_SUFFIX}", phase_word(phase)));
        append_private(&path, &line)
    }

    /// Verify one task's work mechanically, and publish exactly what was verified.
    ///
    /// This is VISION.md §10's steps 3 through 7, and the only door a task has to
    /// [`crate::TaskState::PublishedVerified`]: a task becomes publishable on
    /// mechanical evidence and never on an agent's account of itself. Five things about
    /// it are not free to change:
    ///
    /// - **The candidate is committed before it is verified.** §10's step 4 has the
    ///   final verification run "against the exact candidate commit", which can only be
    ///   true if the candidate exists first. [`git::commit_all`] writes what the
    ///   session changed, [`git::require_clean`] then insists nothing else is left
    ///   unwritten (§10's step 3), and the completion set runs against the tree that
    ///   commit is — so the SHA [`crate::EventKind::PublishStarted`] names, the SHA
    ///   every gate agreed to, and the SHA
    ///   [`crate::EventKind::PublishVerified`] compares with the fetched tip are one
    ///   SHA. ADR-0089 records why the order this task's text prints — clean, then
    ///   verify, then commit — cannot be run as written: uncommitted work is what a
    ///   session always leaves, so [`git::require_clean`] would refuse every task that
    ///   did its job, and a task that changed nothing would have an empty commit
    ///   "proved" by gates that proved only themselves.
    /// - **A dirty tree is a policy failure**, which is §10's own word for it. The
    ///   [`crate::EventKind::VerifyFailed`] row carries
    ///   [`FailureClass::PolicyFailure`] with the paths [`git::require_clean`] named,
    ///   rather than a class [`crate::classify()`] would have to re-derive (ADR-0057).
    ///   No gate is spent and nothing is offered: uncommitted work is not a candidate,
    ///   and a tree that cannot be published must not collect green rows on the way to
    ///   being refused.
    /// - **No path reaches publication without a passing completion set.** Every
    ///   [`crate::EventKind::PublishStarted`] sits behind a
    ///   [`crate::EventKind::VerifyPassed`] for the same attempt, and the retry below
    ///   earns its offer by running the set again rather than by reusing evidence
    ///   gathered against a commit that no longer exists.
    /// - **A rejected push is repaired with git, not with an agent.** Nothing is pushed
    ///   by force, so a branch the remote will not fast-forward comes back as the
    ///   push's own [`Error::Git`]. [`git::rebase_onto_remote`] replays this candidate
    ///   onto the fetched tip and, when the replay applies, the completion set runs from
    ///   scratch against the replayed SHA and the push is tried once more. One retry
    ///   only: a remote another process keeps moving is recovery's problem — the next
    ///   attempt re-preflights and re-bases — and looping here would spend a task's
    ///   whole gate budget on a race this run cannot win.
    /// - **A conflicting divergence stops for a human.**
    ///   [`git::RebaseOutcome::Conflict`] is the one refusal no gate can decide: two
    ///   people's content disagrees, and choosing between them is a decision. The task
    ///   ends with [`crate::EventKind::TaskFailed`] as
    ///   [`FailureClass::GitConflict`] (§7), naming every path the replay could not
    ///   resolve, and no session is started to argue about it.
    ///
    /// Which rows are *not* written here is shaped by the same care. A rejected push
    /// leaves the task in [`crate::TaskState::Publishing`], and that state refuses a
    /// [`crate::EventKind::VerifyFailed`], so a completion set that refuses on the
    /// rerun returns its error with no verdict row: an illegal row is a journal that
    /// cannot replay, which is worse than the hole a dangling gate pair already marks.
    /// For the same reason no [`crate::EventKind::PhaseEntered`] is appended here —
    /// [`Runner::run_phase`] owns entering a phase, and `Publishing` takes a gates
    /// entry only for a later attempt. A gate that could not be started, a tree that
    /// could not be read, and a git call that refused for a reason other than
    /// divergence journal nothing either: nothing was measured, so there is no verdict
    /// to record.
    ///
    /// The last row is worth having because [`git::publish`] does not stop at pushing:
    /// it fetches again and requires the remote's tip to *be* the candidate, so
    /// [`crate::EventKind::PublishVerified`] means §10's step 6 was read rather than
    /// hoped, and the privacy scan ran over this task's range before the push (§11).
    ///
    /// # Errors
    ///
    /// [`Error::Policy`] when the checkout holds work nobody committed, or holds
    /// nothing worth committing — both journalled as the policy verdict §10 names.
    /// [`Error::Gate`] when a completion gate refused, or when the rerun after a replay
    /// refused; a gate that could not be started propagates the refusal from
    /// [`run_completion_set`] with its [`crate::EventKind::GateStarted`] row and no
    /// [`crate::EventKind::GateFinished`] after it. [`Error::Git`] when git refused
    /// something this step cannot repair — an unreadable tree, a refused commit, a
    /// fetch, a push — and for a conflict, as the rebase's own refusal naming the
    /// paths. [`Error::Database`] as [`Recorder::record`], and as
    /// [`run_completion_set`] for the rows it writes.
    pub fn verify_and_publish(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
    ) -> Result<String> {
        let mut under = UnderAttempt::alone(prep, attempt);
        self.publish_the_attempt(&mut under, task)
    }

    /// [`Runner::verify_and_publish`] for the attempt a run is working, which is the
    /// attempt a refusal here has to be explained for.
    ///
    /// The completion set's results are kept whatever the set decided, passing gates
    /// included. §7's "after any remediation, every completion gate reruns from
    /// scratch" is worth nothing if the rerun is believed on the strength of a name:
    /// what the remediation's own account says is which gates ran again in this attempt
    /// and how each came back, and a signature that counted a gate which passed would
    /// stop two identical refusals from looking identical.
    fn publish_the_attempt(&mut self, under: &mut UnderAttempt<'_>, task: &Task) -> Result<String> {
        let prep = under.ground;
        let attempt = under.attempt;
        let candidate = self.commit_candidate(prep, task, attempt)?;
        self.verify_completion(under, task, VerdictRow::Append)?;
        match self.offer(prep, task, attempt, &candidate) {
            Ok(published) => Ok(published),
            Err(refusal) if is_push_refusal(&refusal) => self.republish_after_rebase(under, task),
            Err(refusal) => Err(refusal),
        }
    }

    /// Make the commit the completion set is measured against.
    ///
    /// [`git::commit_all`] stages what the session changed and writes it under the
    /// task's own message — tracked files only, which is why a new file nobody staged
    /// stays the §10 policy failure it reads as instead of becoming part of a candidate
    /// by accident. [`git::require_clean`] then insists the tree holds nothing else: no
    /// half-written file, no build artifact, no scratch note.
    ///
    /// Both refusals are the run's own rule broken, so both are journalled as verdicts.
    /// A git refusal — an index a human has locked, a hook that would not sign, a tree
    /// that could not be read at all — is not a finding about the work: it comes back
    /// unchanged, with no row, because nothing was measured.
    fn commit_candidate(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
    ) -> Result<String> {
        let candidate = self.policy_verdict(
            task.id,
            attempt,
            git::commit_all(&prep.worktree, &task_commit_message(task)),
        )?;
        self.policy_verdict(task.id, attempt, git::require_clean(&prep.worktree))?;
        Ok(candidate)
    }

    /// Run the completion set over the candidate and journal what it decided.
    ///
    /// [`run_completion_set`] is §8's set in the profile's own order, and it journals
    /// its own gate pairs as it runs them (ADR-0080). It is handed [`Prepared::base_sha`]
    /// rather than the candidate because the privacy scan reads the range this task
    /// added: a scan pointed at the whole repository would report every secret the
    /// mainline already holds and say nothing about this diff.
    ///
    /// A gate that ran and refused has answered, so the answer is journalled and
    /// returned in the same words. The class comes from the run rather than from a
    /// guess, and the detail is every refusing gate's own line — unless the state the
    /// journal stands in would refuse that row, which is what `verdict` says.
    fn verify_completion(
        &mut self,
        under: &mut UnderAttempt<'_>,
        task: &Task,
        verdict: VerdictRow,
    ) -> Result<()> {
        let prep = under.ground;
        let attempt = under.attempt;
        let results = run_completion_set(
            &self.profile,
            &prep.worktree,
            &prep.base_sha,
            Some(&mut self.recorder),
        )?;
        under.seen.gates.extend(results.iter().cloned());
        if let Some(refused) = completion_refusal(&self.profile, &results) {
            if verdict == VerdictRow::Append {
                self.verdict(task.id, attempt, refused.class, &refused.detail)?;
            }
            return Err(Error::Gate {
                kind: refused.kind,
                detail: refused.detail,
            });
        }
        self.recorder
            .record(Some(task.id), EventKind::VerifyPassed { attempt })?;
        Ok(())
    }

    /// Offer one candidate to the remote: the offer journalled before the push, the
    /// proof after the read-back.
    ///
    /// [`crate::EventKind::PublishStarted`] comes first because §3's third invariant
    /// puts the row in front of the side effect — a run that died between the two is
    /// found with a commit in the air, which is exactly what
    /// [`crate::TaskState::Publishing`] exists to say. [`git::publish`] then refuses
    /// unless the fetched tip is the candidate, so
    /// [`crate::EventKind::PublishVerified`] can carry that SHA as both the commit and
    /// the remote's SHA, and be checked by anyone who replays the row.
    fn offer(
        &mut self,
        prep: &Prepared,
        task: &Task,
        attempt: AttemptId,
        candidate: &str,
    ) -> Result<String> {
        self.recorder.record(
            Some(task.id),
            EventKind::PublishStarted {
                attempt,
                candidate_sha: candidate.to_owned(),
            },
        )?;
        git::publish(
            &prep.worktree,
            &self.config.mainline_remote,
            &self.config.mainline_branch,
            candidate,
        )?;
        self.recorder.record(
            Some(task.id),
            EventKind::PublishVerified {
                commit: candidate.to_owned(),
                remote_sha: candidate.to_owned(),
            },
        )?;
        Ok(candidate.to_owned())
    }

    /// Repair a push the remote refused: replay, gate again, offer once more.
    ///
    /// Nothing is committed or re-checked on this path. The candidate is already a
    /// commit, and [`git::rebase_onto_remote`] runs with `--no-autostash`, so a tree
    /// holding uncommitted work is a refusal rather than a silent move of that work into
    /// no ref at all: the check [`Self::commit_candidate`] made still stands, and
    /// [`git::commit_all`] would refuse here only because the index is empty.
    ///
    /// The set runs again from scratch, because a replay writes a different commit and
    /// nothing proved against the old tip carries over. The offer that follows is the
    /// second and last — the bound and the reason for it are in
    /// [`Self::verify_and_publish`]. A refusal on this path is the one verdict that
    /// cannot be journalled: the rejected push already moved the task to `Publishing`,
    /// which holds no `VerifyFailed` (see [`VerdictRow`]), so the error is all the
    /// record there is and the dangling gate pair is the hole a reader sees.
    fn republish_after_rebase(
        &mut self,
        under: &mut UnderAttempt<'_>,
        task: &Task,
    ) -> Result<String> {
        let prep = under.ground;
        let attempt = under.attempt;
        let replayed = match self.rebase(prep)? {
            git::RebaseOutcome::Applied { new_sha } => new_sha,
            git::RebaseOutcome::Conflict { paths } => {
                return self.stop_on_conflict(task, &paths);
            }
        };
        self.verify_completion(under, task, VerdictRow::Withhold)?;
        self.offer(prep, task, attempt, &replayed)
    }

    /// Replay this checkout onto the remote's tip, using the run's own remote and
    /// branch rather than whatever the checkout tracks.
    ///
    /// Both names come out of [`Config`] because a rebase onto a branch the project
    /// never named is how a run integrates into somebody else's line of work — the rule
    /// ADR-0041 keeps for every call in [`crate::git`] that could ask a repository what
    /// it means.
    fn rebase(&self, prep: &Prepared) -> Result<git::RebaseOutcome> {
        git::rebase_onto_remote(
            &prep.worktree,
            &self.config.mainline_remote,
            &self.config.mainline_branch,
        )
    }

    /// Stop the task on a divergence git could not replay, naming every path in it.
    ///
    /// This is the refusal with a human in it. The tree is clean, every rule held, and
    /// what stopped the run is two sides wanting different content — so §7's class is
    /// `git_conflict`, the row is [`crate::EventKind::TaskFailed`] (which
    /// [`crate::TaskState::Publishing`] accepts, so the journal replays to `Failed`
    /// rather than leaving a task that an agent will be called back to), and no session
    /// starts to choose. [`git::rebase_onto_remote`] has already aborted the replay, so
    /// the checkout is as it was found for whoever does resolve it.
    ///
    /// The error keeps git's shape rather than becoming [`Error::Policy`]: its argument
    /// vector is the rebase this step attempted and its message names the paths, which
    /// is what a reader acts on. It also avoids the words [`crate::classify()`] reads as
    /// a git that never started — this one started, ran, and stopped on content.
    fn stop_on_conflict(&mut self, task: &Task, paths: &[PathBuf]) -> Result<String> {
        let detail = conflict_words(
            &self.config.mainline_remote,
            &self.config.mainline_branch,
            paths,
        );
        self.recorder.record(
            Some(task.id),
            EventKind::TaskFailed {
                class: FailureClass::GitConflict,
                detail: detail.clone(),
            },
        )?;
        Err(Error::Git {
            args: vec![
                "rebase".to_owned(),
                "--no-autostash".to_owned(),
                upstream_ref(&self.config.mainline_remote, &self.config.mainline_branch),
            ],
            stderr: detail,
        })
    }

    /// Append the verdict a verification run reached, under the task it was for.
    fn verdict(
        &mut self,
        work: TaskId,
        attempt: AttemptId,
        class: FailureClass,
        detail: &str,
    ) -> Result<()> {
        self.recorder.record(
            Some(work),
            EventKind::VerifyFailed {
                attempt,
                class,
                detail: detail.to_owned(),
            },
        )?;
        Ok(())
    }

    /// Journal the verdict a broken rule carries, and hand the caller back the error it
    /// handed in.
    ///
    /// [`Error::Policy`] is a rule of the run's own broken, and §10 names the class for
    /// a dirty tree at verification time, so the class arrives here as data instead of
    /// waiting to be re-derived from the error's words (ADR-0057). Any other error is
    /// not a broken rule, and nothing is journalled for it.
    fn policy_verdict<T>(
        &mut self,
        work: TaskId,
        attempt: AttemptId,
        outcome: Result<T>,
    ) -> Result<T> {
        match outcome {
            Ok(value) => Ok(value),
            Err(refusal) => {
                if let Error::Policy { detail, .. } = &refusal {
                    self.verdict(work, attempt, FailureClass::PolicyFailure, detail)?;
                }
                Err(refusal)
            }
        }
    }

    /// What an attempt looks like at the instant it started: everything that was
    /// decided before the agent was called, and nothing that was observed after.
    fn opened(&self, attempt: AttemptId, task: TaskId, pid: u32, base_sha: &str) -> AttemptRecord {
        AttemptRecord {
            id: attempt,
            task,
            started: OffsetDateTime::now_utc(),
            ended: None,
            model_configured: self.config.model.clone(),
            model_reported: None,
            session_id: None,
            exit_reason: format!("{EXIT_AT_START}{pid}"),
            gates: Vec::new(),
            usage: None,
            base_sha: base_sha.to_owned(),
            candidate_sha: None,
        }
    }

    /// The gates the profile holds, as the words an operator reads them in.
    fn gate_words(&self) -> Vec<String> {
        self.profile
            .gates
            .iter()
            .map(|gate| gate.kind.to_string())
            .collect()
    }

    /// Drive one task end to end: prepare it, work every phase its protocol declares,
    /// verify and publish what resulted, and close the task on the commit the remote
    /// was read back holding.
    ///
    /// VISION.md §6 draws the machine, and [`Runner::prepare`],
    /// [`Runner::begin_attempt`], [`Runner::run_phase`], [`Runner::gate_phase`] and
    /// [`Runner::verify_and_publish`] are its arrows — each already tested against its
    /// own refusals, and each called here in the order that makes a task finish
    /// instead of a task that stopped. What this adds is not a measurement but the
    /// holding of an order, and five things about it are not free to change — each
    /// recorded as ADR-0090, for whoever is tempted to reorder them:
    ///
    /// - [`Runner::prepare`] comes first, and its refusal ends the run. A task refused
    ///   before it began holds no lock and has no checkout, so there is nothing to
    ///   give back and nothing to write behind the preflight's own verdict.
    /// - The ending belongs to the run, not to a session. [`protocol`] appends
    ///   [`Phase::Verify`] and [`Phase::Publish`] to every protocol body, and no agent
    ///   is started for either: [`Runner::verify_and_publish`] is what runs the
    ///   completion set and pushes. So the [`crate::EventKind::PhaseEntered`] row for
    ///   `Verify` is written here — `verify_and_publish` refuses to write an entry
    ///   that is not its own, and [`crate::TaskState::Verifying`] is the state §7's
    ///   remediation is chosen from, so a task whose completion set refused has to be
    ///   standing in it — and no entry is written for `Publish`, because no state
    ///   accepts one.
    /// - A phase that decides by a difference is given the run it differs from.
    ///   [`Runner::gate_phase`] refuses a `red` or `green` phase handed one summary
    ///   rather than two, so the gate that phase declares is run over the checkout as
    ///   the phase starts, before its session has touched anything: the only point at
    ///   which a failure can still be found to be *new*. The summary a phase's own
    ///   gate reached is then what the next phase is compared against.
    /// - A phase's gate runs only when that phase's session says the work is complete.
    ///   A session that left no report, and one that said it stopped short, stop the
    ///   run *before* the gate — the same reason `verify_and_publish` refuses an
    ///   uncommitted tree before any completion gate is spent: a phase with nothing to
    ///   have proved must not collect green gate rows on the way to being refused.
    /// - The state this returns is the journal's, not the run's. It is [`apply`] folded
    ///   over the task's own rows, so a `Done` here is a `Done` the durable record
    ///   replays to; a row the machine would refuse comes back as the refusal it is
    ///   rather than as a finished task its own journal contradicts.
    ///
    /// Whatever the run was holding is given back on every way out of it, including the
    /// ways that end in a refusal, and one case keeps the checkout rather than removing
    /// it: the checkout whose tree still holds work.
    ///
    /// What a refusal earns is §7's one repair, and it is held in
    /// `Runner::work_the_attempts` rather than here: this step's job is to give back
    /// what the run was holding whatever the attempts did, and the loop's job is to
    /// decide whether there is another attempt to give it back after.
    ///
    /// What a refusal does *not* always earn is a repair. Three refusals are pauses
    /// instead — a provider limit, a report that asked a human a question, and a task
    /// that is itself a gate — and each of them is journaled and returned as
    /// [`TaskState::Paused`] rather than run out of a budget or ended as
    /// [`TaskState::Failed`], which is what `docs/CONTRACT.md` §1's exit codes 3, 4
    /// and 5 are the exit codes of. See `Runner::answer_the_refusal` and
    /// `Runner::park_at_the_gate`, both of which this step hands the question to.
    ///
    /// What is still not here is the attempt's own record being closed over what its
    /// phases did: that is a task after this one, and it arrives at a journal this one
    /// leaves in a state recovery can read.
    ///
    /// # Errors
    ///
    /// Every refusal of every step it calls, unchanged: as [`Runner::prepare`] for a
    /// preflight that refused and for the lock and checkout that follow its verdict; as
    /// [`Runner::begin_attempt`]; as [`Runner::run_phase`], including the
    /// [`Error::Policy`] for a session that wrote outside its phase's scope; as
    /// [`Runner::gate_phase`]; and as [`Runner::verify_and_publish`], which is the step
    /// that owns a refused completion gate. Two refusals are this step's own, both
    /// [`Error::NotFound`]: a phase whose session left no report, named by the path the
    /// prompt told it to write, and a phase whose session named the claim it ended on.
    pub fn run_task(&mut self, task: &Task) -> Result<TaskState> {
        self.run_task_with(&crate::paths::process_env, task, &Machine)
    }

    /// [`Runner::run_task`] with the environment the phases' prompts are read from
    /// supplied by the caller.
    ///
    /// Threaded out for the same reason [`Runner::run_phase_with`] is, and for a whole
    /// run rather than one phase: `docs/DESIGN.md` Conventions keeps a test out of the
    /// process environment and out of the operator's real configuration home, and a
    /// task driven end to end cannot be aimed at a scratch configuration home unless
    /// the accessor reaches the run that hands it to every phase it works.
    ///
    /// `clock` is threaded out for the same reason and answers the other half of the
    /// same problem: a wait aimed at a reset the provider named cannot be tested by a
    /// test that either sleeps for it or does not. It is the instant the run is at and
    /// the run's willingness to sit a wait out — see [`Clock`].
    ///
    /// A task that is itself a human gate never reaches [`Runner::prepare`] at all: it
    /// is parked first, because a gate that had taken the repository lock would hold it
    /// for the length of a human's decision.
    fn run_task_with(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        task: &Task,
        clock: &dyn Clock,
    ) -> Result<TaskState> {
        if task.gate.is_some() {
            return self.park_at_the_gate(task);
        }
        let ground = self.prepare(task)?;
        match self.work_the_attempts(env, task, &ground, clock) {
            Ok(state) => {
                self.clear_ground(ground)?;
                Ok(state)
            }
            Err(refusal) => {
                // The refusal the run stopped on is the one whoever receives it can act
                // on, so it comes back as it came. A sweep that faulted leaves the
                // checkout standing, which is VISION.md §7's kept checkout and not a
                // second finding; and the lock is given back either way, because
                // [`Runner::clear_ground`] reaches it whatever the checkout did.
                let _fault = self.clear_ground(ground);
                Err(refusal)
            }
        }
    }

    /// Work the task's attempts until one of them finishes it or §7's bounds are spent.
    ///
    /// One loop holds both halves of §7 because neither means anything alone: the
    /// refusal decides what the next session is told, and the bounds decide whether
    /// there is a next session at all. Within it the steps are in the order §7 gives
    /// them — attempt, refusal, classification, bundle, bounds and breaker, fresh
    /// attempt — and the last of them is this task's whole outcome: a session that never
    /// existed before, launched in the checkout the refused attempt left, carrying that
    /// attempt's evidence in its prompt and no session id of any kind.
    ///
    /// What carries across the loop is exactly what §7 says to preserve, and it is
    /// carried by the [`Prepared`] rather than by anything re-derived: the same worktree,
    /// the same lock, and the attempt records each attempt filed in a directory of its
    /// own, which [`read_evidence`] reads back off disk for the next bundle. What does
    /// *not* carry is anything the refused attempt measured: the [`UnderAttempt`] and the
    /// [`Witness`] inside it are built afresh every iteration, which is how "no cached
    /// gate result survives into the second attempt" is true by construction rather than
    /// by a call that clears something and can be forgotten.
    fn work_the_attempts(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        task: &Task,
        ground: &Prepared,
        clock: &dyn Clock,
    ) -> Result<TaskState> {
        let mut repair: Option<Remediation> = None;
        let mut budget = self.budget();
        loop {
            let attempt = self.begin_attempt(task)?;
            let mut under = UnderAttempt::new(ground, repair.as_ref(), attempt);
            match self.work_the_attempt(env, task, &mut under) {
                Ok(state) => return Ok(state),
                Err(refusal) => {
                    match self.answer_the_refusal(task, &mut under, refusal, &mut budget, clock)? {
                        Answer::Repair(next) => repair = Some(next),
                        Answer::Parked(state) => return Ok(state),
                    }
                }
            }
        }
    }

    /// The budget one run's remediations are held to, from its own configuration.
    ///
    /// §7 bounds remediation by attempts, elapsed time and tokens. Two of the three
    /// come straight out of [`Config`]; the third is derived, and ADR-0092 records
    /// both the mapping and the derivation:
    ///
    /// - **Attempts** come from [`Config::max_remediation_attempts`], the ceiling on
    ///   how many *repairs* a task may have. [`Config::max_attempts`] counts a
    ///   different thing — every attempt a task makes, its first one included — and
    ///   [`should_continue`] compares a counter of refusals against a ceiling on
    ///   repairs, so handing it the total would let a project with `max_attempts = 2`
    ///   and `max_remediation_attempts = 1` have two repairs while its own settings
    ///   said one.
    /// - **Elapsed time** has no setting of its own. One session is bounded by
    ///   [`Config::attempt_timeout_secs`] and one task's sessions by
    ///   [`Config::max_attempts`], so their product is the window this configuration
    ///   has already said it will spend on one task. Past that the run is waiting for
    ///   something its own settings do not contemplate, which is the only thing a
    ///   derived bound has earned the right to stop.
    /// - **Tokens** are unbounded, and said out loud rather than left implicit:
    ///   `Config` holds no token ceiling, and [`Bounds::max_tokens`] exists precisely
    ///   so a bound that cannot be measured stops nothing. The spend is still counted
    ///   ([`Budget::spend`]), so the day the configuration grows a ceiling the figure
    ///   it compares against is already being gathered.
    ///
    /// The breaker takes [`Config::circuit_breaker_threshold`] whole. It counts one
    /// signature rather than every refusal, and one run of one task is where a
    /// signature has the chance to repeat.
    fn budget(&self) -> Budget {
        Budget {
            bounds: Bounds {
                max_attempts: self.config.max_remediation_attempts,
                max_elapsed: whole_seconds(
                    self.config
                        .attempt_timeout_secs
                        .saturating_mul(u64::from(self.config.max_attempts)),
                ),
                max_tokens: None,
            },
            breaker: Breaker::new(self.config.circuit_breaker_threshold),
            refused: 0,
            tokens: 0,
            started: Instant::now(),
            limits_waited: 0,
        }
    }

    /// Answer a refused attempt the way §7 answers one: classify, then bound, then
    /// break, then bundle — and only then let a fresh session be started.
    ///
    /// Eight steps, and every one of them is in this order for a reason a rerun can
    /// check. Three of them end the run with a pause instead of an attempt, and none
    /// of the three ends the task: `docs/CONTRACT.md` §1 counts a limit, a gate and a
    /// question as pauses whose exit codes are 3, 4 and 5, and marks none of them a
    /// failure.
    ///
    /// 1. **A protected path is looked at before a single bound is spent.** An
    ///    attempt that edited `clippy.toml` or `scripts/` rewrote the examination it
    ///    is about to pass (VISION.md §3's fifth invariant), and the row that ends
    ///    the task has to name the file. Spending the bounds first would journal
    ///    `attempts 1 past the 1 bound` over a refusal whose cause is a forbidden
    ///    path: the row would be true about the counter and useless to whoever reads
    ///    it.
    /// 2. **A report that asked a human something parks the attempt that wrote it**,
    ///    read out of the report rather than out of the session's output: an agent
    ///    that stopped at a decision often prints nothing at all, and §3's eighth
    ///    invariant pauses the run on that answer. Nothing is classified first,
    ///    because the ask is the fact and a class derived from it could only agree.
    /// 3. **Everything else is classified**, including a session that never ran —
    ///    §7's first sentence carries no exception for a provider that could not be
    ///    reached, and [`Witness::as_a_session`] is how that case is asked at all.
    /// 4. **A provider limit is waited out or parked with**, with the reset the
    ///    provider named read from what it printed and journalled *before* anybody
    ///    sleeps ([`Runner::wait_or_park_on_the_limit`]). A limit spends no bound: it
    ///    is what the provider asked for and not a repair the task earned.
    /// 5. **The classes that may not loop are answered.** §7 names
    ///    [`FailureClass::ProviderConfiguration`] and [`FailureClass::NeedsInput`] as
    ///    the ones that "pause for the human immediately". `needs_input` is §3's
    ///    eighth invariant and parks like step two, from the session's own words when
    ///    no report asked. [`FailureClass::ProviderConfiguration`] is an invalid
    ///    model or a missing executable that no retry can fix, and the run hands the
    ///    refusal back with **no row added** rather than inventing a `TaskFailed` for
    ///    what `docs/CONTRACT.md` calls a pause; a recovery that stopped this way
    ///    still accounts for itself, because it did try a session.
    ///    [`FailureClass::GitConflict`] is handed back for a different reason and
    ///    files nothing: [`Runner::stop_on_conflict`] has already journalled the
    ///    [`EventKind::TaskFailed`] that ends the task, and an account after that row
    ///    is one the machine has no state to hold.
    /// 6. **The bounds are spent and then consulted.** The refusal is charged whether
    ///    or not it had a session, and the run stops at the bound that says so.
    /// 7. **The breaker is consulted on the signature**, so two different failures
    ///    spend two counts and one failure twice spends one ([`signature`]).
    /// 8. **The bundle is gathered last, from disk.** [`read_evidence`] is asked here
    ///    rather than at the top of the step precisely because the next
    ///    [`Runner::begin_attempt`] files a record of its own: the bundle has to say
    ///    how many attempts *there were*, and a bundle that counted the repair it is
    ///    about to launch is off by one in the one line a session reads first.
    ///
    /// A refusal anywhere in the last step is a fault of the run's own — evidence it could
    /// not read, a tree it could not diff — and comes back as it came. It is not
    /// classified and not repaired: a supervisor that retried its own inability to
    /// read the journal would be spending the budget it is out of.
    fn answer_the_refusal(
        &mut self,
        task: &Task,
        under: &mut UnderAttempt<'_>,
        refusal: Error,
        budget: &mut Budget,
        clock: &dyn Clock,
    ) -> Result<Answer> {
        if !under.seen.policy_edit.is_empty() {
            return self.end_for_policy_edit(task.id, under, refusal);
        }
        if let Some(ask) = under.seen.asked.clone() {
            return Ok(Answer::Parked(self.park_for_input(
                task.id,
                under,
                Some(&ask),
            )?));
        }
        let session = under.seen.as_a_session();
        let class = classify(&session, &under.seen.gates, Some(&refusal));
        if class == FailureClass::ProviderLimit {
            return self.wait_or_park_on_the_limit(task, under, budget, clock);
        }
        if never_looped(class) {
            if class == FailureClass::NeedsInput {
                return Ok(Answer::Parked(self.park_for_input(task.id, under, None)?));
            }
            if class != FailureClass::GitConflict {
                self.file_the_account(task.id, under, PAUSED_ACCOUNT)?;
            }
            return Err(refusal);
        }
        budget.spend(under.seen.session.as_ref());
        if let Decision::Stop(bound) = budget.consult() {
            let detail = bound.to_string();
            return self.end_for_a_spent_bound(task.id, under, refusal, class, &detail);
        }
        let key = signature(class, &under.seen.gates);
        if let BreakerState::Tripped { signature, seen } = budget.breaker.record(&key) {
            return self.end_for_the_breaker(task.id, under, refusal, class, &signature, seen);
        }
        Ok(Answer::Repair(self.bundle_the_repair(task, under, class)?))
    }

    /// Park the run at the human gate `task` is, before anything is started.
    ///
    /// VISION.md §6 gives a gate to a person and not to an agent: it "is never handed
    /// to an agent, produces no commit, and reaches `acknowledged` through
    /// `ktask-rs ack` rather than through publication". So the row comes first and
    /// nothing after it happens — no preflight, no lock, no checkout, no session —
    /// which is the whole of why this is asked before [`Runner::prepare`] rather than
    /// after it: a gate that had taken the repository lock would hold it while a
    /// human decided, and §10 makes that lock the thing the rest of the queue waits
    /// behind.
    ///
    /// The pause is journaled once. A second run over the same gate reads the state
    /// its own rows already reach and answers with it, because [`crate::apply`]
    /// refuses a `Paused` row below a task that is already paused (ADR-0026) and an
    /// operator pressing `run` twice is not a corrupted journal.
    fn park_at_the_gate(&mut self, task: &Task) -> Result<TaskState> {
        let already = self.folded(task.id)?;
        if already.is_paused() {
            return Ok(already);
        }
        self.recorder.record(
            Some(task.id),
            EventKind::Paused {
                reason: PauseReason::HumanGate,
            },
        )?;
        self.folded(task.id)
    }

    /// Park the attempt that asked a human something, and journal the asking.
    ///
    /// The row is the ask when there is one to journal. [`decision_event`] reads a
    /// report that wrote the four parts of a decision request, and its
    /// [`EventKind::DecisionRaised`] is the row the catalog already answers by parking
    /// the attempt at [`PauseReason::Input`] — a second [`EventKind::Paused`] row under
    /// it would be the nested pause ADR-0026 refuses. Everything else takes the plain
    /// row: a session that only said it in its output, and a report whose body was
    /// short of a section, are both an agent stopping at a decision it is not
    /// authorised to make, and §3's eighth invariant pauses the run on that answer
    /// whatever the asking's shape. The words it did write are not lost — the report
    /// is filed in the attempt's own evidence directory and its output is in the rows
    /// above this one.
    ///
    /// §7's account comes before either row for the reason ADR-0091 files it before
    /// any ending: [`EventKind::SelfHealingReport`] belongs to the remediation that
    /// is being worked, and a paused state admits no such row at all.
    fn park_for_input(
        &mut self,
        work: TaskId,
        under: &mut UnderAttempt<'_>,
        ask: Option<&str>,
    ) -> Result<TaskState> {
        let raised = ask.and_then(|words| decision_event(words).ok().flatten());
        self.file_the_account(work, under, PAUSED_ACCOUNT)?;
        self.recorder.record(
            Some(work),
            raised.unwrap_or(EventKind::Paused {
                reason: PauseReason::Input,
            }),
        )?;
        self.folded(work)
    }

    /// Answer a provider limit the way §7 answers one: wait it out, or park with the
    /// wait still owed — and never fail the task over it.
    ///
    /// The instant to wake at is read rather than guessed ([`parse_reset`]), and
    /// [`wait_plan`] chooses between waiting to that instant and a bounded backoff,
    /// from this project's own two ceilings. Then the row: a pause naming the
    /// deadline is written *before* anybody sleeps, because VISION.md §3's third
    /// invariant makes the journal the account of what happened and ADR-0009 makes the
    /// instant in that row the one a restarted run wakes at. Nothing here invents an
    /// instant the provider did not name: a limit that named no reset parks with
    /// `until: None`, which is what the pause state has instead of a lie.
    ///
    /// Three things stop a wait, and all three leave the same parked state rather
    /// than a failure: this run has already waited once ([`Budget::a_wait_is_left`]),
    /// the remediation budget is spent, and the clock will not sit out the plan
    /// ([`Clock::sit_out`] — the answer a caller that must return now installs).
    /// Waiting is not remediation, so it spends none of §7's bounds, and that is
    /// deliberate: a limit charged to the attempt budget would end the task as
    /// `TaskFailed` at a bound its own provider refusal had nothing to do with, and
    /// `docs/CONTRACT.md` §1 makes a limit a pause that must never mark a task failed.
    fn wait_or_park_on_the_limit(
        &mut self,
        task: &Task,
        under: &mut UnderAttempt<'_>,
        budget: &mut Budget,
        clock: &dyn Clock,
    ) -> Result<Answer> {
        self.file_the_account(task.id, under, LIMIT_ACCOUNT)?;
        let now = clock.now();
        let session = under.seen.as_a_session();
        let printed = format!("{}\n{}", session.stderr, session.stdout);
        let reset = limit_message(&printed, &[])
            .as_deref()
            .and_then(|line| parse_reset(line, now));
        let plan = wait_plan(
            reset,
            now,
            whole_seconds(self.config.limit_wait_margin_secs),
            whole_seconds(self.config.limit_max_wait_secs),
        );
        let until = match plan {
            WaitPlan::Deadline { at } => Some(at),
            WaitPlan::Backoff { .. } => None,
        };
        self.recorder.record(
            Some(task.id),
            EventKind::Paused {
                reason: PauseReason::Limit { until },
            },
        )?;
        let parked = self.folded(task.id)?;
        if !budget.a_wait_is_left()
            || matches!(budget.consult(), Decision::Stop(_))
            || !clock.sit_out(plan)
        {
            return Ok(Answer::Parked(parked));
        }
        budget.sat_out_a_limit();
        self.recorder.record(Some(task.id), EventKind::Resumed)?;
        Ok(Answer::Repair(self.bundle_the_repair(
            task,
            under,
            FailureClass::ProviderLimit,
        )?))
    }

    /// End the task whose attempt edited the rules it is judged by.
    ///
    /// Three things in an order that cannot be reordered. The account comes first:
    /// §7 owes one to every recovery, and [`crate::TaskState::Remediating`] admits
    /// its row only while the attempt is a repair — after the row that ends the task
    /// the state is `Failed`, which admits nothing at all. Then the ending, naming
    /// the offending paths and not the whole diff, because whoever reads this row is
    /// being sent to the files that broke the rule. Then the refusal unchanged:
    /// whoever asked for the run is owed the error the attempt actually earned, and
    /// that error already lists the paths.
    fn end_for_policy_edit(
        &mut self,
        work: TaskId,
        under: &mut UnderAttempt<'_>,
        refusal: Error,
    ) -> Result<Answer> {
        let paths = under.seen.policy_edit.clone();
        self.file_the_account(work, under, PROTECTED_PATH_ACCOUNT)?;
        self.recorder
            .record(Some(work), policy_edit_event(&paths))?;
        Err(refusal)
    }

    /// End the task whose remediation spent one of §7's bounds.
    ///
    /// The bound's own words are the row's detail and the account's outcome — one
    /// sentence, in the two places a reader looks for it, with no paraphrase free to
    /// drift from the figure that stopped the run.
    fn end_for_a_spent_bound(
        &mut self,
        work: TaskId,
        under: &mut UnderAttempt<'_>,
        refusal: Error,
        class: FailureClass,
        detail: &str,
    ) -> Result<Answer> {
        self.file_the_account(work, under, detail)?;
        self.recorder.record(
            Some(work),
            EventKind::TaskFailed {
                class,
                detail: detail.to_owned(),
            },
        )?;
        Err(refusal)
    }

    /// End the task whose failure signature repeated past the breaker's threshold.
    ///
    /// [`trip_event`] is the row §7's trip leaves, and it is the row the machine
    /// already answers — `TaskFailed`, from `Remediating` as from `Running` — so
    /// nothing here invents a state for a tripped breaker to park in. The account
    /// still comes first: a trip is the *end* of a recovery, and an end without the
    /// account of what was tried is the half-record §7's report exists to prevent.
    fn end_for_the_breaker(
        &mut self,
        work: TaskId,
        under: &mut UnderAttempt<'_>,
        refusal: Error,
        class: FailureClass,
        signature: &str,
        seen: u32,
    ) -> Result<Answer> {
        self.file_the_account(work, under, BREAKER_ACCOUNT)?;
        self.recorder
            .record(Some(work), trip_event(class, signature, seen))?;
        Err(refusal)
    }

    /// Gather what the next session is told, and name what it is being told.
    ///
    /// Four sources, and three of them are read off disk rather than remembered:
    /// [`read_evidence`] for the attempts that came before, [`git::diff_summary`] for
    /// what this attempt changed against the base it started from, and [`Witness`]
    /// for the gates that ran. [`bundle`] assembles them, because the ceiling
    /// [`Config::failure_bundle_bytes`] sets and the order in which evidence is shed
    /// to meet it are decisions about *evidence*, and a runner that trimmed its own
    /// bundle would be a run that chose which of its findings mattered.
    ///
    /// # What is deliberately absent
    ///
    /// No session id, in any spelling. [`Remediation`] has no field that could hold
    /// one and [`Invocation`] no field that could carry one to an adapter, so §7's
    /// "session resume is never relied on" is not a rule this step has to remember:
    /// the repair is a new session told what the last one was refused for, and the
    /// only path from one attempt's identity to the next would be a field somebody
    /// added later, under a test that would then fail.
    fn bundle_the_repair(
        &self,
        task: &Task,
        under: &UnderAttempt<'_>,
        class: FailureClass,
    ) -> Result<Remediation> {
        let prior = read_evidence(&self.project, task.id)?;
        let diff = git::diff_summary(&under.ground.worktree, &under.ground.base_sha)?;
        Ok(Remediation {
            class,
            bundle: bundle(
                task,
                class,
                &under.seen.gates,
                &diff,
                &prior,
                self.config.failure_bundle_bytes,
            ),
        })
    }

    /// File the one account §7 owes for a recovery, and mark it filed.
    ///
    /// A no-op unless this attempt *is* a recovery, which is what makes the same call
    /// right on both paths that reach it: before the ending of a repair that worked,
    /// and inside the refusal of one that did not. A first attempt has no recovery to
    /// account for — §7's "every recovery produces a self-healing report" is a
    /// sentence about recoveries — and the state machine says so as loudly as this
    /// function does, since [`EventKind::SelfHealingReport`] belongs to the
    /// [`crate::TaskState::Remediating`] whose attempt it names and to no other.
    ///
    /// One account per attempt, whatever else happens. `accounted` is set only after
    /// [`file_report`] succeeded, so a filing that refused left neither a row nor a
    /// file and the next call asks again instead of skipping an account that was never
    /// written; and [`file_report`] itself refuses a second one, so an attempt that
    /// was accounted for twice would be reported rather than papered over.
    fn file_the_account(
        &mut self,
        work: TaskId,
        under: &mut UnderAttempt<'_>,
        outcome: &str,
    ) -> Result<()> {
        let Some(class) = under.repair.map(|repair| repair.class) else {
            return Ok(());
        };
        if under.accounted {
            return Ok(());
        }
        let report = RecoveryReport {
            task: work,
            attempt: under.attempt,
            class,
            repairs: under.repairs(),
            outcome: outcome.to_owned(),
        };
        file_report(&self.project, &mut self.recorder, &report)?;
        under.accounted = true;
        Ok(())
    }

    /// Work one attempt: every phase its protocol declares, then the account it owes if
    /// it is a remediation, then the ending.
    ///
    /// The account comes before [`Runner::finish_the_task`] for a reason the state
    /// machine makes hard to miss. [`crate::EventKind::SelfHealingReport`] is accepted by
    /// [`crate::TaskState::Remediating`] naming that same attempt and by no other state,
    /// and the ending's own `Verify` entry moves the task out of `Remediating`. Filed
    /// after the ending it would be a row the fold refuses — and [`Recorder::record`]
    /// does not ask the machine whether it admits a row, so the refusal surfaces later,
    /// as a projection that reads the task from the row before it (ADR-0091).
    ///
    /// The [`TestSummary`] carried between phases is the summary the previous phase's
    /// gate reached, which is the half of §9's comparison a phase cannot produce for
    /// itself; [`Runner::gate_the_baseline`] supplies the other half, for the phases that
    /// need one and no others.
    fn work_the_attempt(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        task: &Task,
        under: &mut UnderAttempt<'_>,
    ) -> Result<TaskState> {
        let mut carried: Option<TestSummary> = None;
        for spec in protocol::for_task(task, &self.config)?.phases {
            if is_ending(spec.phase) {
                break;
            }
            carried = Some(self.work_one_phase(env, task, under, &spec, carried)?);
        }
        self.file_the_account(task.id, under, GREEN_ACCOUNT)?;
        self.finish_the_task(task, under)
    }

    /// Work one phase from the measurement it starts from to the verdict its gate
    /// reached — and hand that verdict on as the next phase's starting point.
    fn work_one_phase(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        task: &Task,
        under: &mut UnderAttempt<'_>,
        spec: &PhaseSpec,
        carried: Option<TestSummary>,
    ) -> Result<TestSummary> {
        let started_from = match carried {
            Some(summary) => Some(summary),
            None => self.gate_the_baseline(under, task, spec)?,
        };
        let session = self.run_the_session(env, under, task, spec)?;
        Self::earned_its_gate(&session)?;
        self.gate_the_phase(under, task, spec, started_from.as_ref())
    }

    /// The measurement a phase that decides by a difference starts from.
    ///
    /// VISION.md §9 decides red by the expected *new* failure and green by that same
    /// test passing, which is a sentence about two runs of a gate, and
    /// [`Runner::gate_phase`] refuses either phase rather than inventing the missing
    /// half. So the gate the phase declares is run here, over the checkout as the phase
    /// starts.
    ///
    /// Nothing is run for a phase that decides by a gate's exit status, and nothing for
    /// a phase that declares no gate: [`Runner::gate_phase`] refuses the second by the
    /// declaration, which is the refusal it makes on its own and the one worth having.
    /// A red phase whose task claims §9's exception to test-first is measured here all
    /// the same, because the claim can only be judged against what the phase changed —
    /// which is not known until its session has ended — and the alternative to
    /// measuring it is comparing the phase against nothing. The gate the exception
    /// excuses is then a gate that ran and whose pair the journal holds; nothing is
    /// decided by it, and no evidence is filed for it, because a phase's deliverable is
    /// the difference between this run and the one that answers it, and the gate that
    /// decides the phase files the names of both.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the phase declares a gate no command is configured for,
    /// [`Error::Gate`] when the command could not be started or wrote no report its
    /// counts could come from, and [`Error::Database`] as [`Recorder::record`] for the
    /// pair this runs.
    fn gate_the_baseline(
        &mut self,
        under: &mut UnderAttempt<'_>,
        task: &Task,
        spec: &PhaseSpec,
    ) -> Result<Option<TestSummary>> {
        let ground = under.ground;
        if !matches!(spec.phase, Phase::Red | Phase::Green) {
            return Ok(None);
        }
        let Some(kind) = spec.gate else {
            return Ok(None);
        };
        let gate = self.configured_gate(kind)?;
        let result = self.run_declared_gate(task.id, &gate, &ground.worktree)?;
        under.seen.gates.push(result.clone());
        let summary = test_report(&result).ok_or_else(|| no_test_report(&gate, &result))?;
        Ok(Some(summary))
    }

    /// Refuse a phase whose session left the run nothing to gate.
    ///
    /// [`Runner::gate_phase`] turns a phase's work into a verdict, so it is asked of a
    /// phase whose own session says the work is finished. The two other answers stop the
    /// run here — before the gate, before the ending — and each is refused by what it was
    /// found to be rather than by a class re-derived from it (ADR-0057): a report that
    /// was never written is refused by the path the prompt named and the class
    /// [`crate::ReportClaim::Missing`] carried with it, and a session that said it
    /// stopped short is refused by the header it wrote, quoted rather than paraphrased so
    /// the refusal carries what the agent actually claimed.
    ///
    /// Nothing is journalled here. What a phase that stopped has is the rows it reached
    /// itself — [`Runner::run_phase`] wrote its entry, its output and its end — and a
    /// verdict belongs to a gate that did not run. What such an answer does *not* get is
    /// what this step is for: it neither publishes a task nor closes it. Where the run
    /// goes next is the question §3's eighth invariant answers, and
    /// `Runner::answer_the_refusal` answers it — a `NEEDS_INPUT` parks the attempt, and
    /// no refusal invented here is what makes that true.
    fn earned_its_gate(outcome: &PhaseOutcome) -> Result<()> {
        match outcome {
            PhaseOutcome::Claimed {
                report: ReportResult::Done,
                ..
            } => Ok(()),
            PhaseOutcome::Claimed { phase, text, .. } => Err(Error::NotFound {
                what: format!(
                    "the {} phase's finished work: its session ended the report the prompt \
                 named with `{}`, so its gate has nothing to have proved and the task \
                 is not carried to publication on that answer",
                    phase_word(*phase),
                    claim_words(text)
                ),
            }),
            PhaseOutcome::Unreported {
                phase,
                class,
                detail,
                ..
            } => Err(Error::NotFound {
                what: format!(
                    "the report the {} phase's session owed: {detail}, and a phase that never \
                 accounted for itself has nothing its gate could prove, so the run stops \
                 before the ending ({class:?})",
                    phase_word(*phase)
                ),
            }),
        }
    }

    /// The ending every protocol has, which no session is started for: the completion
    /// set, the candidate, the push, and the row that closes the task on the commit the
    /// remote was read back holding.
    ///
    /// [`crate::EventKind::TaskDone`] carries that SHA and no other, which is what makes
    /// [`crate::TaskState::Done`] mean *the task was closed on the commit the remote
    /// holds*: the state refuses the row unless the two agree.
    ///
    /// The ending goes through [`Runner::publish_the_attempt`] rather than the
    /// [`Runner::verify_and_publish`] a caller outside a run comes through, for the same
    /// reason the phases do: the completion set's answer is the evidence §7 classifies a
    /// refusal of the ending from, and a refusal that reached the loop through the door
    /// that drops its gate results would be sorted as an agent's fault rather than the
    /// gate's.
    fn finish_the_task(&mut self, task: &Task, under: &mut UnderAttempt<'_>) -> Result<TaskState> {
        let attempt = under.attempt;
        self.recorder.record(
            Some(task.id),
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
        )?;
        let commit = self.publish_the_attempt(under, task)?;
        self.recorder
            .record(Some(task.id), EventKind::TaskDone { commit })?;
        self.folded(task.id)
    }

    /// Give back everything the run was holding, whatever it is returning.
    ///
    /// The checkout first, then the lock, and both always reached: [`Prepared`] holds
    /// [`crate::lock::RepoLock`] by value, and asking it for the lock back is what makes
    /// a lock this run no longer owns a reported fault rather than the silence of a
    /// destructor. One refusal does not hide the other question — both are asked, and
    /// the first refusal is the one handed back.
    fn clear_ground(&self, ground: Prepared) -> Result<()> {
        let Prepared { worktree, lock, .. } = ground;
        let swept = self.sweep_checkout(&worktree);
        let released = lock.release();
        swept.and(released)
    }

    /// Remove the task's checkout, unless it holds work.
    ///
    /// [`git::remove_worktree`] has no force, on purpose: it refuses a checkout with
    /// anything uncommitted in it rather than delete the work an attempt left. So the
    /// checkout is asked whether it is empty first, and *no* is VISION.md §7's kept
    /// checkout — the work the next attempt is told to read, and the directory
    /// [`git::create_worktree`] hands that attempt back instead of cutting a second one
    /// beside it. Refusing to remove a checkout that *was* empty is a git fault, and
    /// comes back as one: a run that cannot tell what it left behind is not a run that
    /// should have started.
    fn sweep_checkout(&self, worktree: &Path) -> Result<()> {
        if git::is_clean(worktree)? {
            git::remove_worktree(&self.project.root, worktree)?;
        }
        Ok(())
    }

    /// The state the task's own rows fold to, read out of the journal.
    ///
    /// The run answers "where is this task" with the projection's answer rather than a
    /// value it remembered, which is what makes VISION.md §3's third invariant a check
    /// instead of a slogan: a row the state machine refuses is returned as the
    /// [`Error::InvalidTransition`] it is. This is the fold
    /// [`crate::Journal::rebuild_state`] does, over one task's rows rather than the
    /// whole journal's — and the rows whose task is `NULL` are left out here too, for
    /// the same reason: they are about the queue, and they move no task.
    fn folded(&self, work: TaskId) -> Result<TaskState> {
        let journal = Journal::open_for(&self.project)?;
        let mut state = TaskState::Queued;
        for row in journal.events_for(work)? {
            state = apply(&state, &row.kind)?;
        }
        Ok(state)
    }
}

/// Whether `phase` is part of the ending every protocol ends with — and which the run
/// therefore works itself, because no session is started for either of the two.
const fn is_ending(phase: Phase) -> bool {
    matches!(phase, Phase::Verify | Phase::Publish)
}

/// Whether §7 forbids launching another session after a refusal of this class.
///
/// Two of the nine classes are named by §7 itself — `provider_configuration` and
/// `needs_input` — and both pause for a human rather than spend a repair: an
/// invalid model and an unresolved product decision are both answers a retry cannot
/// produce, and §3 makes the second one a pause rather than a failure.
/// [`FailureClass::GitConflict`] joins them here for a reason that is the runner's
/// rather than §7's: the conflict is refused by [`Runner::stop_on_conflict`], which
/// journals the [`EventKind::TaskFailed`] that ends the task on the spot, and an
/// attempt started after a row that ended one is a transition out of a state
/// [`crate::apply`] holds terminal. Every other class is what a repair is for.
fn never_looped(class: FailureClass) -> bool {
    matches!(
        class,
        FailureClass::ProviderConfiguration | FailureClass::NeedsInput | FailureClass::GitConflict
    )
}

/// The header an agent's report opened with, in the report's own words.
///
/// [`crate::parse_report`] read that line before this could be reached — a report whose
/// first line with content is none of the three claims is [`Error::Corrupt`] by then —
/// so quoting it says what the agent claimed rather than re-spelling a table that lives
/// in another module and would drift from it.
fn claim_words(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// The status a session that never started reports, for the one reader that needs
/// a session where none happened.
///
/// §7's first sentence asks for a classification *before any recovery is
/// attempted*, and a provider that could not be reached refused before there was
/// an [`Outcome`] to classify. [`classify()`] takes an [`Outcome`] because a session
/// is usually the loudest evidence there is; handing it the empty one below is the
/// only way to ask the question at all. `-1` is the shell's own "could not be
/// run", which no CLI returns as a status it means on the platform this supervisor
/// runs on, and nothing else reads it: the class comes from the refusal handed in
/// beside it, and [`Outcome::usage`] stays `None` so that no token figure is
/// invented along with the status (ADR-0049).
const NEVER_RAN: i32 = -1;

/// What the account of a repair says when its own gates all came back green.
///
/// It is filed before the ending rather than after it, because
/// [`crate::TaskState::Remediating`] is the only state that admits
/// [`crate::EventKind::SelfHealingReport`] and the ending's own
/// [`crate::EventKind::PhaseEntered`] leaves that state — so the sentence cannot
/// claim the completion set passed, because at the instant it is written nothing
/// has asked the completion set anything. What it can say, and says, is which
/// gates it actually watched.
const GREEN_ACCOUNT: &str = "every gate this repair ran came back green; the completion set, the \
                             candidate and the publication are journalled after this account, in \
                             this attempt's own rows";

/// What the account of a repair says when the refusal it earned is one §7 will not
/// loop.
const PAUSED_ACCOUNT: &str = "the repair stopped to put a question to a human, and the run paused on \
                              that answer instead of launching another session after it";

/// What the account of a repair says when the refusal it earned was a provider limit.
///
/// It says nothing about *when* the run goes on, because the account is filed before
/// the wait is planned and, more than that, before anyone knows whether this run is
/// the one that waits: an account that promised a wait the run then parked instead of
/// taking would be a record of something that did not happen.
const LIMIT_ACCOUNT: &str = "the provider refused for a usage limit, and the run stopped to wait it \
                             out rather than launch another session against it";

/// What the account of a repair says when the refusal it earned was a protected path.
const PROTECTED_PATH_ACCOUNT: &str = "the repair touched a path it is judged by, and the task ended \
                                      before anything was measured against the edited rule";

/// What the account of a repair says when the breaker ended the task it was trying.
const BREAKER_ACCOUNT: &str = "the same failure signature came back a second time, the circuit \
                              breaker tripped, and no further session was launched";

/// The one line every account of a repair opens with, whatever else it tried.
///
/// §7 makes the fresh session *the* repair mechanism — "every remediation launches
/// a fresh provider session" — so it is a repair that was attempted, in every
/// account, and it is the one that cannot be left out of a report read a week
/// later by someone asking what the supervisor actually did.
const FRESH_SESSION_REPAIR: &str = "a fresh provider session, seeded with the failure bundle and \
                                    handed no session id of the attempt it replaces";

/// The heading a repair's bundle is handed under.
///
/// [`crate::context::build_prompt`] is left alone (ADR-0075) and the bundle is
/// appended below this line, so a session reads the same documents its predecessor
/// read and then the supervisor's own account of why it is being asked again.
const BUNDLE_HEADING: &str = "Remediation: the previous attempt was refused. Its evidence, in a \
                              compact bundle, follows.";

/// What one attempt was seen to do, kept for the refusal it may earn.
///
/// [`classify()`] and [`signature`] both read what an attempt *measured*, and none of
/// it survives on disk in the shape they want: an [`Outcome`] is what a provider
/// answered, a [`GateResult`] is what a command came back with, and the journal
/// holds rows about them rather than the values. Rebuilding them from the journal
/// would be reading the run's own summary and calling it evidence, so the attempt
/// keeps its answers while it is worked and hands them over once, at the refusal.
///
/// This is the whole of what a refusal carries out of an attempt. It is deliberately
/// *not* what an attempt carries into the next one: [`UnderAttempt`] is built afresh
/// for every attempt, so §7's "no cached evidence survives a file change" is true by
/// construction — a value that was never built cannot be reused.
#[derive(Debug, Default)]
struct Witness {
    /// What this attempt's session answered, or `None` when it never had one: a
    /// provider that could not be reached refused before there was a session.
    session: Option<Outcome>,
    /// Every gate this attempt ran, in the order it ran them, passing ones included
    /// — [`signature`] counts only the refusals among them, and an account says
    /// which gates *ran again* rather than only which ones refused.
    gates: Vec<GateResult>,
    /// The protected paths this attempt's session touched, when it touched any.
    ///
    /// [`check_no_policy_edit`] names them and refuses; they travel out with the
    /// refusal so the row that ends the task can send a human to those files rather
    /// than to the attempt's whole diff.
    policy_edit: Vec<PathBuf>,
    /// The report this attempt's session wrote when the report asked a human for a
    /// decision, in the agent's own words.
    ///
    /// Held apart from [`Witness::session`] because a report is not session output,
    /// and [`classify()`] reads only the latter: a session that wrote a
    /// `NEEDS_INPUT` report and printed nothing is the ordinary shape of an agent
    /// that stopped at a decision, and reading it out of the report is the only way
    /// to see it. [`decision_event`] turns the text into the row the pause is.
    asked: Option<String>,
}

impl Witness {
    /// The session to hand [`classify()`] now that this attempt is over.
    ///
    /// One that ran answers for itself. One that never ran has nothing to answer
    /// with, and §7 still requires its refusal to be classified before anything is
    /// recovered — so what comes back is an empty session carrying [`NEVER_RAN`].
    fn as_a_session(&self) -> Outcome {
        self.session.clone().unwrap_or_else(|| Outcome {
            exit_code: NEVER_RAN,
            stdout: String::new(),
            stderr: String::new(),
            usage: None,
            session_id: None,
            model_reported: None,
        })
    }
}

/// What one refused attempt got back from §7's round of questions.
///
/// Two answers, because a refusal has two endings and they are not the same kind of
/// thing: §7's repair — a fresh session told what the last one was refused for — and a
/// pause, which is the run stopping somewhere on purpose with the state it stopped at
/// already journalled. Naming both here is what keeps a pause from being spelled as a
/// refusal: an [`Error`] out of that round is a fault, and VISION.md §3's eighth
/// invariant and `docs/CONTRACT.md` §1 both insist that a limit, a gate and a question
/// are not faults.
enum Answer {
    /// Another attempt, seeded with this bundle.
    Repair(Remediation),
    /// The run stops here, at the state its own journal already reaches.
    Parked(TaskState),
}

/// The instant a run is at, and the waiting it is willing to do.
///
/// VISION.md §7's response to a provider limit is a wait aimed at an instant, and an
/// instant is not something a run can be tested against: a test that slept for the
/// margin would take the margin, and a test that did not sleep would not have tested
/// the wait. So the two halves of a wait are asked separately — what time is it, and
/// will you sit this out — and a run is handed the answers rather than reading a clock
/// of its own.
///
/// It is a parameter of [`Runner::run_task_with`] and not a sixth field of a
/// [`Runner`], because a run is made of five things and no more. A clock the run held
/// would be a clock whose answer belonged to whichever step happened to ask, and the
/// instant a pause is planned against and the instant it sleeps to have to be the same
/// one the caller decided.
trait Clock {
    /// The instant now, which is what a reset the provider named is measured against.
    fn now(&self) -> OffsetDateTime;

    /// Sit `plan` out, and report whether this run actually waited.
    ///
    /// `false` means the plan was not waited out and is still owed: the pause stays
    /// where its row put it, and whoever resumes the run later wakes at the instant the
    /// journal already holds. A caller that must answer now — a CLI whose exit code says
    /// the work is paused (CONTRACT.md §1's code 3) rather than showing that it is
    /// asleep — installs a clock that says no.
    fn sit_out(&self, plan: WaitPlan) -> bool;
}

/// The machine's own clock: the real instant, and a thread that really sleeps.
struct Machine;

impl Clock for Machine {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn sit_out(&self, plan: WaitPlan) -> bool {
        let until = match plan {
            WaitPlan::Deadline { at } => at,
            WaitPlan::Backoff { wait } => OffsetDateTime::now_utc().saturating_add(wait),
        };
        let mut left = until - OffsetDateTime::now_utc();
        while left.is_positive() {
            std::thread::sleep(Duration::try_from(left).unwrap_or(Duration::MAX));
            left = until - OffsetDateTime::now_utc();
        }
        true
    }
}

/// The repair one refusal asked for, before the session that carries it out exists.
///
/// Two fields because a remediation is exactly two facts: what the failure was
/// called, and what the next session is told about it. The class is carried rather
/// than re-derived later because §7 classifies *before* recovering, and because
/// [`crate::RecoveryReport`] has to name the failure the repair set out to answer —
/// an account that re-classifies from what is left by then is a different question
/// answered twice, and the two answers are free to disagree.
struct Remediation {
    /// The class [`classify()`] settled before any recovery was attempted.
    class: FailureClass,
    /// The compact bundle §7 seeds a fresh session with: classification, gate
    /// output, diff summary and prior attempt evidence, fitted to
    /// [`Config::failure_bundle_bytes`].
    bundle: String,
}

impl Remediation {
    /// Lay this bundle under the heading that says whose account of the refusal it is.
    ///
    /// Appended, never woven in: the documents a first attempt was handed are the
    /// documents this one gets, and what it gets *extra* is the evidence of the
    /// attempt it replaces. There is no field of an [`Invocation`] for a session id
    /// to come back in, which is the mechanical half of §7's "session resume is
    /// never relied on".
    fn seed(&self, prompt: &mut String) {
        prompt.push_str("\n\n");
        prompt.push_str(BUNDLE_HEADING);
        prompt.push('\n');
        prompt.push_str(&self.bundle);
        prompt.push('\n');
    }
}

/// One attempt being worked: the ground it works on, what it has been seen to do,
/// and whether it exists because a previous attempt was refused.
///
/// §7 gives a remediation two contradictory-sounding requirements — carry the
/// refused attempt's worktree and evidence forward, and carry none of its
/// measurements forward — and a struct is where that split can be made visible
/// rather than merely intended. `ground` and `repair` come across the loop;
/// `seen` and `accounted` are built fresh every iteration.
///
/// It is a *view* held for the length of one attempt, which is why it borrows: the
/// checkout, the base and the lock belong to [`Prepared`], and a second owner of
/// them would be a second answer to "who gives the lock back".
struct UnderAttempt<'a> {
    /// The checkout, the base and the lock — §7's "preserve the worktree and all
    /// prior attempt evidence across remediation" is a fact about this field
    /// surviving from one attempt to the next.
    ground: &'a Prepared,
    /// Which attempt this is: `1` for a task's first, and the number
    /// [`crate::TaskState::Remediating`] holds and the evidence directory is named
    /// for when it is a repair.
    attempt: AttemptId,
    /// The repair this attempt is carrying out, or `None` for a first attempt.
    ///
    /// Its presence answers "is this a remediation", which is the question that
    /// decides whether the prompt is seeded with a bundle and whether an account of
    /// a recovery is owed at the end. `None` is not a repair with an empty bundle.
    repair: Option<&'a Remediation>,
    /// What this attempt has been seen to do, from the first gate it ran.
    seen: Witness,
    /// Whether this attempt has filed its account of itself already.
    ///
    /// §7's "every recovery produces a self-healing report" is one report, and
    /// [`file_report`] refuses a second one for the same attempt as
    /// [`Error::Policy`]. A refusal can arrive on a path that already filed one —
    /// the account is filed before the ending, and the ending can still refuse — so
    /// the flag is what makes the second call a no-op rather than a new failure.
    accounted: bool,
}

impl<'a> UnderAttempt<'a> {
    /// The attempt `attempt` of `ground`, carrying `repair` when it is a repair.
    fn new(ground: &'a Prepared, repair: Option<&'a Remediation>, attempt: AttemptId) -> Self {
        Self {
            ground,
            attempt,
            repair,
            seen: Witness::default(),
            accounted: false,
        }
    }

    /// The view [`Runner::run_phase`], [`Runner::gate_phase`] and
    /// [`Runner::verify_and_publish`] work from.
    ///
    /// A caller outside a run can say which checkout and which attempt, and cannot
    /// say anything about a repair: it did not decide one. Stepping through
    /// [`UnderAttempt::new`] with `None` is the whole of the difference, which is
    /// the point — the single-phase doors behave exactly as they did before a run
    /// could be retried, because the state that makes a retry different lives in the
    /// run and not in them.
    fn alone(ground: &'a Prepared, attempt: AttemptId) -> Self {
        Self::new(ground, None, attempt)
    }

    /// The "attempted repairs" §7's account has to carry, in the order they were
    /// tried.
    ///
    /// Empty for an attempt that is not a repair: there was nothing to attempt, and
    /// [`crate::RecoveryReport::repairs`] being empty means "nothing was tried", so
    /// an invented line would be read as a repair that was tried and did not work.
    /// For a repair, the fresh session always comes first and one line per gate it
    /// reruns follows, because "every completion gate reruns from scratch" is only
    /// auditable if the account says which gates ran and how each came back.
    fn repairs(&self) -> Vec<String> {
        if self.repair.is_none() {
            return Vec::new();
        }
        let mut tried = vec![FRESH_SESSION_REPAIR.to_owned()];
        tried.extend(self.seen.gates.iter().map(|gate| {
            let verdict = if gate.passed { "green" } else { "red" };
            format!(
                "the {} gate ran again from scratch and came back {verdict}",
                gate.kind.as_str()
            )
        }));
        tried
    }
}

/// What a remediation is allowed to spend, and what it has spent so far.
///
/// §7 bounds remediation by attempts, elapsed time and tokens, and holds the three
/// against [`Bounds`] rather than against three separate counters so the order the
/// bounds are asked in lives in one place ([`should_continue`]) and a stopped
/// remediation leaves the same reason behind every reader reaches.
///
/// It is one value per run rather than one per attempt, because a bound that is
/// re-zeroed for every attempt bounds nothing: the whole point is the total a task's
/// failure may still cost.
struct Budget {
    /// The three ceilings, as this project's configuration spells them.
    bounds: Bounds,
    /// The breaker §7 asks for: repeated identical signatures, not repeated
    /// refusals.
    breaker: Breaker,
    /// Refusals this run has been refused so far.
    refused: u32,
    /// Tokens every session of this run has reported, held as [`should_continue`]
    /// wants it: a figure nobody reported stays `0` rather than becoming an estimate.
    tokens: u64,
    /// When this run started spending, for the elapsed bound.
    started: Instant,
    /// Provider limits this run has already waited out, against
    /// [`LIMIT_WAITS_PER_RUN`].
    ///
    /// Deliberately not one of §7's three bounds and never handed to
    /// [`should_continue`]: a wait is what the provider asked for, not a repair the
    /// task earned, and charging it to the attempt budget would let a limit spend the
    /// retries a later genuine failure needs and end that failure's task at a bound
    /// nothing refused. It exists because a run that re-hit its limit the instant it
    /// woke would otherwise wait again, and again, inside one run.
    limits_waited: u32,
}

impl Budget {
    /// Charge one more refused attempt to this budget.
    ///
    /// The refusal is counted whatever it was — including one that never had a
    /// session — because the bound §7 bounds is the number of times a task has been
    /// refused, not the number of sessions that got far enough to report. What was
    /// spent comes from that attempt's session alone, and adds nothing when there
    /// was no session or it reported no figures.
    fn spend(&mut self, session: Option<&Outcome>) {
        self.refused = self.refused.saturating_add(1);
        self.tokens = self.tokens.saturating_add(
            session
                .and_then(|outcome| outcome.usage.as_ref())
                .and_then(Usage::total_tokens)
                .unwrap_or(0),
        );
    }

    /// The bound these figures have spent, or [`Decision::Continue`].
    ///
    /// Elapsed time is measured from [`Budget::started`] rather than summed from the
    /// sessions' own durations: a session that hung for an hour cost an hour, and
    /// the bound exists to stop a run waiting for it.
    fn consult(&self) -> Decision {
        should_continue(
            &self.bounds,
            self.refused,
            whole_seconds(self.started.elapsed().as_secs()),
            self.tokens,
        )
    }

    /// Whether this run may still sit out a provider limit.
    ///
    /// One wait per run is the hard limit VISION.md §7's "within hard limits" asks
    /// for: the second limit in one run is the provider saying the reset the first
    /// wait was planned around was wrong, which is a fact about the provider a
    /// waiting supervisor cannot settle and a screen can.
    fn a_wait_is_left(&self) -> bool {
        self.limits_waited < LIMIT_WAITS_PER_RUN
    }

    /// Charge the wait this run just sat out to this budget.
    fn sat_out_a_limit(&mut self) {
        self.limits_waited = self.limits_waited.saturating_add(1);
    }
}

/// How many provider limits one run may sit out before it parks with the wait still
/// owed instead of sitting out another.
///
/// One, because the point of the wait is VISION.md §7's "waits until the exact reset
/// time": a limit that came back after its own promised reset had passed is not the
/// same promise being kept, and a run that re-waits every time it is refused would
/// look, from a screen, exactly like a run that is working.
const LIMIT_WAITS_PER_RUN: u32 = 1;

/// `spent` whole seconds, in the signed span [`Bounds`] is spelled in.
///
/// The conversion saturates rather than truncating, because the alternative is a
/// 64-bit count of seconds silently becoming a different number inside the bound
/// that is supposed to stop a run: a span too large for `i64` is past any bound a
/// configuration can name, and reads as one.
fn whole_seconds(spent: u64) -> time::Duration {
    time::Duration::seconds(i64::try_from(spent).unwrap_or(i64::MAX))
}

/// A task that has been proved worth starting, and the ground it starts on.
///
/// Three things, none of which the next step can settle for itself: which checkout
/// the work happens in, which commit every later measurement of that work is taken
/// against, and which lock keeps another run from publishing underneath either.
///
/// It is handed back rather than kept by the [`Runner`], because VISION.md §3 makes
/// the journal — not a struct — the account of what state a task is in. The
/// lifetime is the point of holding it here at all: [`crate::lock::RepoLock`] gives
/// the lock back when this is dropped, so "this task's work happened under the
/// repository lock" is a fact about a value's scope rather than about a call
/// somebody remembered to make at the end.
#[derive(Debug)]
pub struct Prepared {
    /// The task's own checkout: one entry in the repository's managed task
    /// directory, named after the task, detached at [`Prepared::base_sha`].
    pub worktree: PathBuf,
    /// The tip `<remote>/<branch>` had when the preflight's fetch moved it, which
    /// is what [`crate::git::changed_paths`], the privacy gate and the final
    /// comparison against the remote are all measured against.
    pub base_sha: String,
    /// The project's repository lock, held since before this checkout was cut.
    ///
    /// Private, and with no getter: [`crate::lock::RepoLock::release`] consumes the
    /// lock, and a public field would let a caller move it out or drop it early
    /// while this run was still publishing under it.
    lock: lock::RepoLock,
}

impl Prepared {
    /// The abandoned lock this preparation took over, if it took one over.
    ///
    /// A lock left by a holder that has provably gone is taken without waiting, and
    /// [`crate::lock`] is explicit that a takeover is never silent: the reason a
    /// lock is left behind is usually the reason the work before it did not finish,
    /// which is what the attempt's evidence and whoever reads it back need to see.
    /// [`None`] is the ordinary answer — this run waited behind nobody.
    #[must_use]
    pub fn reclaimed(&self) -> Option<&lock::Reclaimed> {
        self.lock.reclaimed()
    }
}

/// What one phase's session left behind: its own account of itself, and the work
/// it actually made.
///
/// Two answers, because after a provider exits the file the prompt named is either
/// there or not there, and a third answer — assume it went well — is what VISION.md
/// §3's invariant 4 forbids. [`Runner::run_phase`] holds both halves at once, which
/// is the point of the type: an outcome that carried only the agent's claim would
/// describe what was *said*, and the one thing a supervisor is built to know is what
/// changed. `changed` is therefore the checkout's own diff against the base the
/// preflight recorded — tracked edits and never-staged files together — and it is
/// measured before either arm is built, so the list a scope refusal quotes
/// ([`Error::Policy`], naming every path that broke the rule) and the list this arm
/// carries are one measurement rather than two.
///
/// Nothing here is a verdict. The gates, the push and the fetched remote outrank the
/// claim (VISION.md §3's invariants 4 and 7), and no arm of this enum can be
/// constructed from a phase that has not run: a phase that was refused on the way
/// arrives as an [`Error`], not as an outcome with a `DONE` in it.
#[derive(Debug)]
pub enum PhaseOutcome {
    /// The session wrote the report the prompt named, and this is what it said —
    /// read by [`crate::parse_report`], so the header is one of the three claims and
    /// nothing else the file held has been interpreted.
    Claimed {
        /// Which phase of the protocol was worked, as the declaration named it.
        phase: Phase,
        /// Which run of the task worked it: the same id the session's rows carry.
        attempt: AttemptId,
        /// What the agent said it achieved. A claim, never a verdict.
        report: ReportResult,
        /// The whole text of the report. A `NEEDS_INPUT` body is what
        /// [`crate::decision_request`] reads, and opening the file again for it would
        /// be a second read of a file a fast session may have replaced.
        text: String,
        /// Every path the session left changed below its checkout, measured against
        /// [`Prepared::base_sha`] and already known to lie inside the phase's scope —
        /// [`Runner::run_phase`] refuses before this arm can be built otherwise.
        changed: Vec<PathBuf>,
    },
    /// The session ended and left no report. A failure the run acts on, reported with
    /// the class and the path it was found to be missing from.
    Unreported {
        /// Which phase was worked, so whoever reads the failure knows what stopped.
        phase: Phase,
        /// Which run of the task left it unwritten.
        attempt: AttemptId,
        /// The class VISION.md §7's recovery policy is read from, as
        /// [`crate::ReportClaim::Missing`] gave it: reported, never re-derived from
        /// the absence (ADR-0057).
        class: FailureClass,
        /// The path the prompt told the agent to write to. A refusal nobody can
        /// locate is a refusal nobody can act on.
        path: PathBuf,
        /// The one-line account of the refusal, naming `path`.
        detail: String,
    },
}

/// What a command decided, named rather than numbered.
///
/// `docs/DESIGN.md` *Other fixed types* fixes these variants and
/// `docs/CONTRACT.md` §1 fixes what each of them means. The numbers §1's table
/// pairs them with are deliberately not part of this type, and no function in
/// this crate produces one: `run`, `retry` and the rest say *what happened*, and
/// the process boundary that turns that into a status is the CLI alone. A core
/// that returned `3` had already chosen which interface it was answering, and the
/// TUI — the primary interface — has no exit code to show a paused queue with.
///
/// The division that carries the weight is §1's own: [`RunOutcome::Drained`] is
/// the only answer that says the work finished, [`RunOutcome::TaskFailed`] the
/// only one that says a task failed, and the three between them are pauses. A
/// provider limit, a human gate and a question a person owes stop the queue
/// without refusing anything (VISION.md §6), and none of them may mark its task
/// `failed` — which is why they are three variants and never a
/// [`TaskState::Failed`] wearing a different coat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Every task the run was asked to work reached a terminal success state, in
    /// order, with its predecessor's completion proved before it started.
    Drained,
    /// A task failed with its remediation budget spent, which stops everything
    /// behind it: a successor cannot start until its predecessor is complete.
    TaskFailed {
        /// The task that failed, which is the one `retry --task` is aimed at.
        task: TaskId,
    },
    /// The command itself was used wrongly — bad arguments, a malformed task, no
    /// registered project. Nothing ran, so nothing failed and nothing is paused.
    Usage {
        /// What was wrong, in the words the operator acts on: the argument, the
        /// missing section, or the `ktask-rs init` that would have made one.
        detail: String,
    },
    /// A provider's plan limit stopped the run. The world said pause: the work is
    /// neither done nor wrong, and the same attempt resumes when the ceiling lifts.
    ProviderLimit {
        /// When the ceiling lifts, as the provider named it. `None` when it named
        /// no time and the run's own back-off chose the wait — an invented instant
        /// would be a deadline a resumed run slept to for no reason.
        until: Option<OffsetDateTime>,
    },
    /// The run reached a task that is itself a human gate. Only a person moves it,
    /// and the queue behind it waits (VISION.md §3's invariant 7).
    HumanGate {
        /// The gate task that stopped the queue, which `ack` is aimed at.
        task: TaskId,
    },
    /// A task put a decision to a person and the run refused to spend a guess on
    /// the answer (VISION.md §3's invariant 8).
    NeedsInput {
        /// The task whose question is owed an answer, which `resolve` is aimed at.
        task: TaskId,
    },
    /// Something stopped the run from outside it. What it had already journaled is
    /// durable, so this answer means *resume*, not *start over*.
    Interrupted,
}

/// The name the repository registers `task`'s checkout under.
///
/// One name per task rather than one per attempt: VISION.md §7 requires a
/// remediation to keep the worktree it stopped in, and [`crate::git`] makes one
/// name one directory, so a name carrying an attempt number would orphan the
/// checkout whose evidence the next attempt is told to read. `task-7` is the shape
/// ADR-0043 anticipates, and it is one path component, which is all
/// [`crate::git::create_worktree`] accepts.
fn worktree_name(task: TaskId) -> String {
    format!("task-{task}")
}

/// The message one task's candidate carries: the queue position it delivers and the
/// title it was queued under, so `git log` on the mainline says which task a commit is
/// for without opening a journal.
fn task_commit_message(task: &Task) -> String {
    format!("Task {}: {}", task.id, task.title())
}

/// Whether a refusal is the push itself saying no, which is the one refusal §10's step 5
/// has a repair for.
///
/// [`crate::git::publish`] runs three commands and says which one refused, so a fetch
/// that could not reach the remote, a read-back that disagrees after a push that
/// worked, and a push the remote would not fast-forward are three different facts that
/// reach this call the same way. Only the third is a divergence a replay can settle; the
/// other two come back as they are rather than being followed by a rebase that could not
/// help.
fn is_push_refusal(refusal: &Error) -> bool {
    matches!(refusal, Error::Git { args, .. }
        if args.first().is_some_and(|word| word == "push"))
}

/// The ref a repair replays onto, spelled the way [`crate::git`] spells it.
fn upstream_ref(remote: &str, branch: &str) -> String {
    format!("refs/remotes/{remote}/{branch}")
}

/// The sentence a replay that stopped on content leaves behind, in the row and in the
/// error alike: which ref was replayed onto, and every path the two sides want
/// differently.
///
/// The paths are git's own listing, quoting and all (ADR-0041), because the reader's
/// next command is a git command in that checkout. The wording is chosen to keep
/// [`crate::classify()`]'s phrase for a git that never started out of it: this replay
/// started, ran, and stopped on content.
fn conflict_words(remote: &str, branch: &str, paths: &[PathBuf]) -> String {
    let listed = paths
        .iter()
        .map(|path| format!("`{}`", path.display()))
        .collect::<Vec<String>>()
        .join(", ");
    format!(
        "the replay onto `{remote}/{branch}` stopped on a conflict in {listed}, which is a \
         decision this run is not allowed to make"
    )
}

/// The lines a session printed, one per row: a break ends a line rather than
/// opening another, so text that printed one thing and finished leaves one row and
/// never a second empty one, while a blank line an agent wrote on purpose is kept.
fn session_lines(printed: &str) -> Vec<String> {
    printed
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches('\n').to_owned())
        .collect()
}

/// How an attempt's record says it is running, before it has an ending.
///
/// [`AttemptRecord::exit_reason`] is not optional and an attempt that is merely
/// running has not exited, so the one true sentence available is the one that
/// says who is running it: this const beside the same pid the journal row
/// carries, which is what a recovery reader cross-checks first.
const EXIT_AT_START: &str = "started as pid ";

/// What an attempt is told at the moment it starts: nothing.
///
/// Assembling the context document is the step after this one (VISION.md §6).
/// Filing an empty one now is deliberate — the artifact is part of the directory
/// that says "this attempt existed", and a later step writes over it with the
/// document it assembled rather than inventing the directory after the fact.
const CONTEXT_AT_START: &str = "";

/// The base `task` was given to start from, and the highest attempt number it has.
///
/// Both come from one pass over the task's own rows, because both are facts about
/// what this project has already durably said rather than about what the process
/// holding this [`Runner`] remembers:
///
/// - The base is the last [`EventKind::PreflightPassed`] the task has, which is
///   what [`AttemptRecord::base_sha`] means by "the one `PreflightPassed`
///   recorded". Reading `git::head_sha` instead would let an attempt name a tree
///   no check ever proved green, and the pair of them would then disagree in the
///   journal — the exact contradiction VISION.md §3's third invariant exists to
///   make impossible. A task with no such row is refused.
/// - The attempt number is the highest `AttemptStarted` number the task has, so a
///   retry continues the task's numbering instead of restarting it, whoever the
///   process asking happens to be.
///
/// A row that is neither of those two kinds is not a fact about a task's opening
/// and is passed over.
///
/// # Errors
///
/// [`Error::Database`] and [`Error::Corrupt`] as [`Journal::events_for`], and
/// [`Error::NotFound`] when the task has no recorded base.
fn recorded_base(journal: &Journal, task: TaskId) -> Result<(String, u32)> {
    let mut base: Option<String> = None;
    let mut last = 0;
    for row in journal.events_for(task)? {
        match row.kind {
            EventKind::PreflightPassed { base_sha } => base = Some(base_sha),
            EventKind::AttemptStarted { attempt, .. } => {
                last = std::cmp::max(last, attempt.get());
            }
            _ => {}
        }
    }
    base.map(|base_sha| (base_sha, last))
        .ok_or_else(|| Error::NotFound {
            what: format!(
                "task {task}'s base: no `PreflightPassed` row recorded one, so this \
                 task has no commit an attempt may be based on"
            ),
        })
}

impl fmt::Debug for Runner {
    /// The run as an operator needs it in a panic report and a log line: which
    /// project, which adapter, and which gates.
    ///
    /// Written by hand because `Box<dyn Provider>` has no [`fmt::Debug`] to
    /// forward to, and it shows what an operator acts on rather than everything
    /// the run happens to hold. `adapter` is the built adapter's own name and
    /// `configured` is the word that selected it; the two are printed apart
    /// because their disagreeing is the fault worth seeing. The gates are the
    /// words a report line begins with, in the order the profile runs them.
    ///
    /// The rest is deliberately left out, and the trailing `..` says so: the
    /// whole [`Config`] is sixty settings a reader can read from the project's own
    /// document, and a live journal connection and a bus of rings have no answer
    /// to print. Nothing here is where a repair looks; the journal is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runner")
            .field("project", &self.project.id)
            .field("configured", &self.config.provider)
            .field("adapter", &self.provider.name())
            .field("gates", &self.gate_words())
            .finish_non_exhaustive()
    }
}

/// Which of VISION.md §6's five checks a finding belongs to.
///
/// The five are the five the document names, and the order of the variants is
/// the order they are asked in: cheapest first, the answer that goes stale
/// fastest last. The name is what an operator reads in a failure and what a
/// report line begins with, so it is one lower-case word like
/// [`crate::GateKind::as_str`], not a rewording of the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightCheck {
    /// The adapter the run holds answers for the provider the configuration names.
    Provider,
    /// Free space on the state filesystem above `min_free_disk_bytes`.
    DiskSpace,
    /// The remote fetched, its mainline tip resolved, and the checkout clean.
    Mainline,
    /// The configured `baseline_command` ran and passed.
    Baseline,
    /// The repository lock of the project's state directory is acquirable.
    Lock,
}

impl PreflightCheck {
    /// The check as an operator reads it in a report line and a log column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::DiskSpace => "disk",
            Self::Mainline => "mainline",
            Self::Baseline => "baseline",
            Self::Lock => "lock",
        }
    }
}

impl fmt::Display for PreflightCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One check's answer, with the evidence that answers it.
///
/// Two variants rather than a `passed: bool` beside an `Option<FailureClass>`,
/// because the pair has one state a bool cannot express: a refusal with no class
/// is a refusal nothing can respond to, and VISION.md §7 classifies *before* any
/// recovery is attempted. A [`CheckOutcome::Refused`] cannot be built without
/// naming the class, which is where the requirement actually lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The check asked its question and the answer was yes.
    Passed {
        /// Which check answered.
        check: PreflightCheck,
        /// What it saw, in the words that get read back from a journal.
        detail: String,
    },
    /// The check asked its question, the answer was no, and the taxonomy says
    /// what kind of no it was.
    Refused {
        /// Which check refused.
        check: PreflightCheck,
        /// The class VISION.md §7 answers this refusal with. Chosen from the
        /// cause of the refusal rather than from the check: the two arms of the
        /// mainline check, and the three of the baseline check, are different
        /// kinds of failure that happen to be asked by one question.
        class: FailureClass,
        /// What it saw, in the words that get read back from a journal.
        detail: String,
    },
}

impl CheckOutcome {
    /// Which check gave this answer.
    #[must_use]
    pub const fn check(&self) -> PreflightCheck {
        match self {
            Self::Passed { check, .. } | Self::Refused { check, .. } => *check,
        }
    }

    /// Whether the check's rule held.
    #[must_use]
    pub const fn passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// The class of a refusal, or `None` for a check that passed.
    #[must_use]
    pub const fn class(&self) -> Option<FailureClass> {
        match self {
            Self::Passed { .. } => None,
            Self::Refused { class, .. } => Some(*class),
        }
    }

    /// The evidence, in the words the check wrote.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Passed { detail, .. } | Self::Refused { detail, .. } => detail,
        }
    }
}

impl fmt::Display for CheckOutcome {
    /// One line: the check, the verdict and its class, and what was seen.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passed { check, detail } => write!(f, "{check}: passed — {detail}"),
            Self::Refused {
                check,
                class,
                detail,
            } => write!(f, "{check}: refused ({class:?}) — {detail}"),
        }
    }
}

/// What a preflight decided, and everything it saw on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    /// Every check that ran, in the order it ran, ending at the first refusal.
    ///
    /// A check that never ran has no row here, which is why a report is read in
    /// order rather than counted: three rows ending in a refusal says the last
    /// two checks were never asked, and nothing here claims an answer for them.
    pub checks: Vec<CheckOutcome>,
    /// The tip of `<remote>/<branch>` the fetch brought back, which is the commit
    /// the work will be based on and the base every later gate is measured
    /// against.
    ///
    /// Empty until the mainline check has fetched and resolved it: a report that
    /// refused before or at that check has no tip to name, and inventing one would
    /// hand a later stage a base for work that was never allowed to start. A report
    /// that passed always carries it, because passing means that check ran and said
    /// yes.
    pub base_sha: String,
}

impl PreflightReport {
    /// Whether every check that ran said yes.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(CheckOutcome::passed)
    }

    /// The refusal that ended the checks, if there was one.
    ///
    /// The first, because the first refusal is where the checks stop: a report
    /// cannot hold two answers to a question that was never asked twice.
    #[must_use]
    pub fn refusal(&self) -> Option<&CheckOutcome> {
        self.checks.iter().find(|outcome| !outcome.passed())
    }

    /// Every check that ran, one line each, oldest first.
    ///
    /// This is what a refusal's journal row carries, so the record of a preflight
    /// that stopped says what the earlier checks saw as well as what stopped it.
    #[must_use]
    pub fn evidence(&self) -> String {
        self.checks
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The journal record this report's verdict asks for.
    ///
    /// Handed back rather than appended: the run that journaled
    /// [`crate::EventKind::PreflightStarted`] owns the recorder that answers it,
    /// and `state::apply` ends a task on a `PreflightFailed`. See the module
    /// documentation for what this module journals itself.
    #[must_use]
    pub fn event(&self) -> EventKind {
        for outcome in &self.checks {
            if let CheckOutcome::Refused { class, .. } = outcome {
                return EventKind::PreflightFailed {
                    class: *class,
                    detail: self.evidence(),
                };
            }
        }
        EventKind::PreflightPassed {
            base_sha: self.base_sha.clone(),
        }
    }
}

/// Run the five checks VISION.md §6 requires before a task may start.
///
/// The checks run in the order [`PreflightCheck`] lists them, and the first
/// refusal returns the report as it stands: a run told about a full disk does not
/// also need a baseline's minutes spent behind it.
///
/// # Errors
///
/// [`crate::Error::Database`] when the project's state directory is not there —
/// like [`crate::journal`] and [`crate::lock`], this module writes into a
/// directory registration owns and never conjures one; [`crate::Error::Config`]
/// when the configuration describes a profile that cannot run, which is the
/// mandatory-gate rule of [`profile_from`]; [`crate::Error::Database`] when a
/// gate's own row could not be journaled, because a check that ran without its
/// evidence is a check that did not happen.
///
/// A check that refuses is never an error. See the module documentation.
pub fn preflight(
    project: &Project,
    config: &Config,
    provider: &dyn Provider,
) -> Result<PreflightReport> {
    let mut recorder = Recorder::new(Journal::open_for(project)?);
    let mut report = PreflightReport {
        checks: Vec::new(),
        base_sha: String::new(),
    };

    if stop_at_first_refusal(&mut report, check_provider(config, provider)) {
        return Ok(report);
    }
    let floor = config.min_free_disk_bytes;
    if stop_at_first_refusal(&mut report, check_disk(&project.state_dir, floor)) {
        return Ok(report);
    }
    let fetched = check_mainline(
        &project.root,
        &config.mainline_remote,
        &config.mainline_branch,
    );
    if let Some(tip) = fetched.tip {
        report.base_sha = tip;
    }
    if stop_at_first_refusal(&mut report, fetched.outcome) {
        return Ok(report);
    }
    let baseline = check_baseline(config, &project.root, &mut recorder)?;
    if stop_at_first_refusal(&mut report, baseline) {
        return Ok(report);
    }
    if stop_at_first_refusal(&mut report, check_lock(&project.state_dir)) {
        return Ok(report);
    }
    Ok(report)
}

/// Record one check's answer, and say whether it ended the checks.
fn stop_at_first_refusal(report: &mut PreflightReport, outcome: CheckOutcome) -> bool {
    let refused = !outcome.passed();
    report.checks.push(outcome);
    refused
}

/// The answer to "is this the adapter the run was configured to use?".
///
/// What a `&dyn Provider` can say without being invoked is its name and its
/// [`Capabilities`], and those two are all this check asks. [`Provider::invoke`]
/// is the only door from here to "the executable is there and answered", and
/// asking through it spends a session — the one thing a state whose whole job is
/// to precede spending must not do. So the rule checked here is the rule the
/// evidence depends on: a run holding a `codex` adapter while the configuration
/// names `dummy` would file every later gate under a CLI that never ran the work,
/// which is VISION.md §7's `provider_configuration` family and is never fixed by
/// retrying. Whether the named CLI is installed is a question for the provider
/// layer's own detection and for `ktask-rs doctor` (VISION.md §4); ADR-0082
/// records that gap rather than papering over it.
fn check_provider(config: &Config, provider: &dyn Provider) -> CheckOutcome {
    if provider.name() != config.provider {
        return CheckOutcome::Refused {
            check: PreflightCheck::Provider,
            class: FailureClass::ProviderConfiguration,
            detail: format!(
                "this run holds a `{}` adapter while the configuration names `{}`; its \
                 evidence would be filed under a CLI that never ran the work",
                provider.name(),
                config.provider
            ),
        };
    }
    CheckOutcome::Passed {
        check: PreflightCheck::Provider,
        detail: format!(
            "`{}` answers for itself: {}",
            config.provider,
            capability_words(provider.capabilities())
        ),
    }
}

/// What an adapter's three capability answers add up to, in the words a report
/// line carries.
fn capability_words(capabilities: Capabilities) -> String {
    let mut answered = Vec::new();
    if capabilities.structured_output {
        answered.push("structured output");
    }
    if capabilities.model_selection {
        answered.push("model selection");
    }
    if capabilities.usage_telemetry {
        answered.push("usage telemetry");
    }
    if answered.is_empty() {
        return "nothing beyond a prompt in and text out".to_owned();
    }
    answered.join(", ")
}

/// The answer to "is there room left to write the evidence this run exists to
/// produce?".
///
/// The filesystem the *state directory* sits on is what is asked, because that is
/// the filesystem a journal, a failure bundle and an attempt's evidence are
/// written to, and `min_free_disk_bytes` is documented against exactly that. What
/// a run may fill is `blocks_available` — not the kernel's larger free count,
/// whose reserved blocks are space a run cannot write into — times the size one of
/// the filesystem's blocks counts.
fn check_disk(state_dir: &Path, floor: u64) -> CheckOutcome {
    let refused = |detail: String| CheckOutcome::Refused {
        check: PreflightCheck::DiskSpace,
        class: FailureClass::EnvironmentFailure,
        detail,
    };
    match statvfs::statvfs(state_dir) {
        Err(failure) => refused(format!(
            "the filesystem below `{}` could not be asked how full it is: {failure}",
            state_dir.display()
        )),
        Ok(filesystem) => {
            let Some(block) = bytes_per_block(&filesystem) else {
                return refused(format!(
                    "the filesystem below `{}` reported no block size, so its free space \
                     cannot be counted",
                    state_dir.display()
                ));
            };
            let free = u128::from(filesystem.blocks_available()) * block;
            if free >= u128::from(floor) {
                return CheckOutcome::Passed {
                    check: PreflightCheck::DiskSpace,
                    detail: format!(
                        "{free} bytes are free below `{}`, above the {floor} the \
                         configuration demands",
                        state_dir.display()
                    ),
                };
            }
            refused(format!(
                "{free} bytes are free below `{}`, which is below the {floor} the \
                 configuration demands",
                state_dir.display()
            ))
        }
    }
}

/// How many bytes one of a filesystem's blocks counts, counted in `u128` because
/// a block count times a block size is a number a `u64` can overflow on a large
/// array.
///
/// The fragment size is the one a `statvfs` caller means; the block size is the
/// fallback a filesystem that reports no fragment size leaves. Zero from both is
/// an answer that cannot be multiplied into bytes, which is why this returns an
/// [`Option`] rather than leaving a caller to divide by what it did not get.
fn bytes_per_block(filesystem: &statvfs::Statvfs) -> Option<u128> {
    let fragment = u128::from(filesystem.fragment_size());
    let block = u128::from(filesystem.block_size());
    if fragment > 0 {
        Some(fragment)
    } else if block > 0 {
        Some(block)
    } else {
        None
    }
}

/// The mainline check's answer, and the tip it read while answering.
///
/// The tip is carried apart from the verdict so that a refusal can leave it out:
/// `base_sha` names the commit work will be based on, and a mainline check that
/// refused — whether the remote would not answer, held no such branch, or the
/// checkout was dirty — has not established a base for anything. What git said
/// about a refusal is in the refusal's own detail, which is where an operator
/// reads it.
struct Mainline {
    /// What the three git questions add up to.
    outcome: CheckOutcome,
    /// `<remote>/<branch>`'s tip, which is only known when the whole check passed.
    tip: Option<String>,
}

/// Ask the three questions the mainline check is: fetch the remote, read its
/// mainline tip, and ask the checkout whether it is clean.
///
/// The tip is read from the ref the fetch moved rather than from `HEAD`, because
/// VISION.md §6's clean fetched mainline is a statement about the remote and about
/// the index — and a checkout standing behind `origin` is ordinary, which is why
/// the two halves of that sentence carry two different classes. A remote that will
/// not answer, a branch it has never held, and a git that refuses to speak are all
/// [`FailureClass::GitConflict`]: the remote does not agree with the run. A tree
/// with uncommitted work in it is [`FailureClass::PolicyFailure`], which is what
/// VISION.md §7 calls "the tree was dirty at verification time".
fn check_mainline(root: &Path, remote: &str, branch: &str) -> Mainline {
    let refused = |class: FailureClass, detail: String| Mainline {
        outcome: CheckOutcome::Refused {
            check: PreflightCheck::Mainline,
            class,
            detail,
        },
        tip: None,
    };
    if let Err(failure) = git::fetch(root, remote) {
        return refused(
            FailureClass::GitConflict,
            format!("`{remote}` could not be fetched: {failure}"),
        );
    }
    let reference = format!("refs/remotes/{remote}/{branch}");
    let Ok(tip) = git::git(root, &["rev-parse", "--verify", &reference]) else {
        return refused(
            FailureClass::GitConflict,
            format!(
                "`{remote}` was fetched, but it holds no `{branch}`: `{reference}` names nothing"
            ),
        );
    };
    match git::status_porcelain(root) {
        Err(failure) => refused(
            FailureClass::GitConflict,
            format!(
                "the checkout at `{}` refused to say whether it is clean: {failure}",
                root.display()
            ),
        ),
        Ok(records) if !records.is_empty() => refused(
            FailureClass::PolicyFailure,
            format!(
                "the checkout at `{}` is not clean: {}",
                root.display(),
                records.join(", ")
            ),
        ),
        Ok(_) => Mainline {
            outcome: CheckOutcome::Passed {
                check: PreflightCheck::Mainline,
                detail: format!(
                    "`{remote}` was fetched, its `{branch}` tip is `{tip}`, and the checkout \
                     at `{}` is clean",
                    root.display()
                ),
            },
            tip: Some(tip),
        },
    }
}

/// Run the configured baseline gate, and journal the pair that says it ran.
///
/// This is the only check with a command behind it, so it is the only one with
/// rows of its own: [`crate::journal`] admits no catalog entry that can carry a
/// check's result, and the pair is written under no task, exactly as
/// [`crate::run_completion_set`] writes a gate that belongs to no task yet. A gate
/// that could not be started leaves a [`EventKind::GateStarted`] with no answer
/// after it — the pair `run_completion_set` documents as saying this gate never
/// completed — and is refused rather than returned as an error, because a program
/// that is not installed is VISION.md §7's machine being the wrong one and the run
/// has to be told which kind of wrong it met.
///
/// # Errors
///
/// [`crate::Error::Config`] from [`profile_from`] when the configuration cannot
/// describe a runnable profile at all, and [`crate::Error::Database`] when one of
/// the two rows was refused.
fn check_baseline(config: &Config, root: &Path, recorder: &mut Recorder) -> Result<CheckOutcome> {
    let profile = profile_from(config)?;
    let Some(configured) = profile.get(GateKind::Baseline) else {
        return Ok(CheckOutcome::Passed {
            check: PreflightCheck::Baseline,
            detail: "no `baseline_command` is configured, so there was no baseline to prove"
                .to_owned(),
        });
    };
    let gate = configured.clone();
    recorder.record(
        None,
        EventKind::GateStarted {
            gate: GateKind::Baseline,
        },
    )?;
    match run_gate(&gate, root, None) {
        Ok(result) => {
            recorder.record(
                None,
                EventKind::GateFinished {
                    result: result.clone(),
                },
            )?;
            Ok(baseline_verdict(&gate, &result))
        }
        Err(failure) => Ok(CheckOutcome::Refused {
            check: PreflightCheck::Baseline,
            class: FailureClass::EnvironmentFailure,
            detail: format!(
                "the baseline gate `{}` could not be started: {failure}",
                command_words(&gate)
            ),
        }),
    }
}

/// What a baseline run's [`GateResult`] means for the check that asked for it.
fn baseline_verdict(gate: &Gate, result: &GateResult) -> CheckOutcome {
    let detail = baseline_detail(gate, result);
    if result.passed {
        return CheckOutcome::Passed {
            check: PreflightCheck::Baseline,
            detail,
        };
    }
    CheckOutcome::Refused {
        check: PreflightCheck::Baseline,
        // The same class a completion gate earns for the same run, so the two halves
        // of the supervisor cannot disagree about a timeout.
        class: verdict_class(result),
        detail,
    }
}

/// The one line a baseline run's evidence fits into: the command, how long it ran,
/// how it ended, and the last thing it said.
fn baseline_detail(gate: &Gate, result: &GateResult) -> String {
    gate_detail("baseline gate", gate, result)
}

/// The one line one gate's run fits into: what it was called, how long it ran, how it
/// ended, and the last thing it said.
///
/// `noun` is what the caller calls the gate — the baseline, a phase's own check, a
/// member of the completion set — because a report that says only "the gate" is no use
/// to a reader who cannot tell which of the five ran. The other four facts are what
/// every gate answers with, which is why a refusal reads the same wherever it came from.
fn gate_detail(noun: &str, gate: &Gate, result: &GateResult) -> String {
    let said = match last_words(result) {
        Some(line) => format!(", and its last line was `{line}`"),
        None => String::new(),
    };
    format!(
        "the {noun} `{}` ran for {} ms and {}{said}",
        command_words(gate),
        result.duration_ms,
        ended_words(result, gate.timeout_secs)
    )
}

/// How a gate's run ended, in the words an operator reads in a report line.
fn ended_words(result: &GateResult, timeout_secs: u64) -> String {
    if result.passed {
        return "passed".to_owned();
    }
    if result.timed_out {
        return format!("ran out of its {timeout_secs} s budget");
    }
    match (result.exit_code, result.signal) {
        (Some(code), _) => format!("exited with code {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        (None, None) => "produced no status at all".to_owned(),
    }
}

/// The last thing a gate's command said, from the stream a failing command
/// explains itself on. [`None`] when it said nothing, which is what a silent
/// `/bin/true` does.
fn last_words(result: &GateResult) -> Option<&str> {
    let spoken = if result.stderr.trim().is_empty() {
        &result.stdout
    } else {
        &result.stderr
    };
    spoken
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// A gate's command as the one string a report line names it by.
fn command_words(gate: &Gate) -> String {
    gate.command.join(" ")
}

/// The class a gate's run earns, which is the same answer for a baseline, a phase's own
/// check and a member of the completion set: a command that ran out of its budget is no
/// verdict about the code but the machine's answer about itself, and anything that ran
/// to a verdict and refused is the code's. ADR-0082 sets the rule, and
/// [`crate::classify()`] applies the same one, so the two halves of the supervisor
/// cannot disagree about a timeout.
fn verdict_class(result: &GateResult) -> FailureClass {
    if result.timed_out {
        FailureClass::EnvironmentFailure
    } else {
        FailureClass::VerificationFailure
    }
}

/// Whether a refusing completion set is allowed to journal its verdict.
///
/// The answer is the state the journal stands in, not a preference: `Running` holds a
/// [`crate::EventKind::VerifyFailed`] and hands it to `Failed`, while `Publishing`
/// refuses one — a rejected push leaves the task in `Publishing`, and the only rows it
/// accepts besides its own are a gate pair, a passing verdict, and
/// [`crate::EventKind::TaskFailed`]. A row the projection cannot apply is a journal that
/// no longer replays, which is the far worse outcome: the run would have written its own
/// evidence out of existence. So the rerun after a replayed push reports its refusal as
/// the returned error and lets the dangling [`crate::EventKind::GateStarted`] mark where
/// it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerdictRow {
    /// A verdict about the code: append the row the attempt's state holds.
    Append,
    /// The state would refuse the row: return the refusal with nothing appended.
    Withhold,
}

/// What a completion-set run that refused decided, in the three forms the run needs:
/// which gate the error names, which class the row carries, and the one line both hold.
struct CompletionRefusal {
    /// The lower-case word the first refusing gate calls itself, as [`Error::Gate`]
    /// holds it.
    kind: String,
    /// The class that first refusing run earns.
    class: FailureClass,
    /// Every refusing gate's own line, in the order they refused.
    detail: String,
}

/// Pick the refusing gates out of a completion set's results and say what their run
/// decided, or [`None`] when every gate agreed.
///
/// Each result is matched with the profile's own copy of the gate that produced it,
/// because a [`GateResult`] carries no command and no budget: a refusal that cannot name
/// the command that refused or the budget it ran out of tells a reader nothing to act
/// on. Every refusal is described, not only the first, since a set that stopped early
/// still answers for the mandatory gate it ran behind the refusal.
fn completion_refusal(profile: &Profile, results: &[GateResult]) -> Option<CompletionRefusal> {
    let refused: Vec<(&Gate, &GateResult)> = results
        .iter()
        .filter(|result| !result.passed)
        .filter_map(|result| profile.get(result.kind).map(|gate| (gate, result)))
        .collect();
    let (gate, result) = *refused.first()?;
    Some(CompletionRefusal {
        kind: gate.kind.to_string(),
        class: verdict_class(result),
        detail: refused
            .iter()
            .map(|(gate, result)| gate_detail("completion gate", gate, result))
            .collect::<Vec<String>>()
            .join("; "),
    })
}

/// The directory of one attempt's phase evidence, inside its evidence directory.
const PHASES_DIR: &str = "phases";

/// The suffix of one phase's evidence file, named for the [`Phase`] it records.
const JSONL_SUFFIX: &str = ".jsonl";

/// The mode bits of the phase-evidence directory: owner-only, the rule every level of
/// the state directory is kept to (VISION.md §11).
const PHASE_DIR_MODE: u32 = 0o700;

/// The mode bits of a phase's evidence file. Gate output is the least shareable thing a
/// run produces, and it is the one artifact that holds a test's own words verbatim.
const PHASE_FILE_MODE: u32 = 0o600;

/// The prefix that makes a tree hash a tree hash rather than somebody else's digest of
/// a similar string.
const TREE_PREFIX: &[u8] = b"ktask-tree-v1\0";

/// What a phase compares its own gate run against, which its declaration alone decides.
///
/// Three answers, because the phases decide themselves three different ways. Red and
/// green are differences — a test that newly fails, a name that newly passes — and so
/// each carries the summary the phase started from. Every other phase a protocol holds
/// is decided by one run's verdict, and the [`Comparison::Gate`] arm carrying no summary
/// is what keeps [`Runner::gate_phase`] honest about that: a refactor handed no
/// comparison is not a red phase missing one, and refusing it would be a rule no
/// declaration wrote.
#[derive(Clone, Copy)]
enum Comparison<'a> {
    /// The summary the red phase started from, which its new failure has to be new.
    Red(&'a TestSummary),
    /// The names the phase started from failing, which green has to leave passing.
    Green(&'a TestSummary),
    /// Nothing to compare: the run's own verdict decides.
    Gate,
}

/// Pair a phase with what it has to differ from, refusing the phases that need one.
///
/// The refusal comes before any gate row, and before any command is started, because
/// there is no finding to make: a red phase with nothing to compare did not fail to
/// break a test, it was asked a question with both halves missing. VISION.md §3's
/// invariant 3 is what the ordering protects — a run cannot end up with the journal's
/// account of a gate whose answer the caller could not have read.
fn comparison(phase: Phase, before: Option<&TestSummary>) -> Result<Comparison<'_>> {
    match (phase, before) {
        (Phase::Red, Some(prior)) => Ok(Comparison::Red(prior)),
        (Phase::Green, Some(prior)) => Ok(Comparison::Green(prior)),
        (Phase::Red | Phase::Green, None) => Err(no_prior_summary(phase)),
        _ => Ok(Comparison::Gate),
    }
}

/// Decide one phase from the run its gate made, naming what the verdict named.
///
/// The names are the phase's deliverable rather than a by-product: they are what §9
/// files as RED evidence, what green is confirmed against, and what an inspector shows
/// a person who has to decide whether the test that failed is the test that was meant
/// to. The lists themselves belong to [`protocol::verify_red`] and
/// [`protocol::verify_green`], which is where the two rules are written and tested;
/// this chooses which rule a phase is decided by and adds the run's own verdict to it.
fn phase_verdict(
    way: Comparison<'_>,
    gate: &Gate,
    result: &GateResult,
    after: &TestSummary,
) -> Result<Vec<String>> {
    match way {
        Comparison::Red(prior) => protocol::verify_red(prior, after),
        Comparison::Green(prior) => confirm_green(prior, gate, result, after),
        Comparison::Gate if result.passed => Ok(Vec::new()),
        Comparison::Gate => Err(gate_refused(gate, result)),
    }
}

/// Confirm a green phase fixed what it was for and broke nothing else.
///
/// Both halves of §9 step 4 in one call — the named test passing and no other test
/// newly failing — and then the gate's own verdict on top. That order matters: the
/// named refusal says which test this phase was for, which is the sentence an operator
/// can act on, where "the command exited 1" only says the phase did not finish. And the
/// gate's verdict still has to agree, because a `cargo test` that died after printing
/// its counts, or a workspace whose second test binary failed to build, prints names
/// that all pass and still refuses.
///
/// The names handed to [`protocol::verify_green`] are every name the previous run left
/// failing, which after a phase that ran the same command is the set §9 means: a green
/// phase answers for the whole targeted list, not only for the one test red added.
fn confirm_green(
    prior: &TestSummary,
    gate: &Gate,
    result: &GateResult,
    after: &TestSummary,
) -> Result<Vec<String>> {
    protocol::verify_green(&prior.failures, after)?;
    decided_by_gate(gate, result)?;
    Ok(prior.failures.clone())
}

/// Refuse unless the gate's own verdict is that it passed.
fn decided_by_gate(gate: &Gate, result: &GateResult) -> Result<()> {
    if result.passed {
        return Ok(());
    }
    Err(gate_refused(gate, result))
}

/// Refuse a phase by the run that refused it, in the words the preflight's baseline
/// line already uses for the same facts: which command, how long, how it ended, and the
/// last thing it said.
fn gate_refused(gate: &Gate, result: &GateResult) -> Error {
    Error::Gate {
        kind: gate.kind.to_string(),
        detail: gate_detail("gate", gate, result),
    }
}

/// Refuse a phase whose gate ran and wrote nothing a summary could be read out of.
///
/// This is the case a count can be wrong about: output with no `test result:` line is
/// [`None`] from [`crate::parse_cargo`] rather than a run with zero failures
/// (ADR-0039), and reading it as `0 failed` would pass a red phase that ran nothing and
/// green phases that built nothing. The command is refused with what it did end with,
/// because "there is no report" and "here is the line it failed on" are the two halves
/// of the same answer.
fn no_test_report(gate: &Gate, result: &GateResult) -> Error {
    let said = match last_words(result) {
        Some(line) => format!(", and its last line was `{line}`"),
        None => String::new(),
    };
    Error::Gate {
        kind: gate.kind.to_string(),
        detail: format!(
            "the gate `{}` ran for {} ms and {}, and wrote no test report its counts could \
             come from{said}",
            command_words(gate),
            result.duration_ms,
            ended_words(result, gate.timeout_secs)
        ),
    }
}

/// Refuse a phase whose declaration names no gate, by the phase that had none.
fn no_gate_declared(phase: Phase) -> Error {
    Error::NotFound {
        what: format!(
            "the gate the {} phase declares: its declaration names none, so nothing decides \
             it mechanically and no run of any command is recorded",
            phase_word(phase)
        ),
    }
}

/// Refuse a phase that decides by a difference and was given nothing to differ from.
fn no_prior_summary(phase: Phase) -> Error {
    Error::NotFound {
        what: format!(
            "the test summary the {} phase compares against: {} decides by what changed \
             between two runs of the gate, and only one run was handed to it",
            phase_word(phase),
            phase_word(phase)
        ),
    }
}

/// The phase as its evidence file is named and its refusals quote it: lower-case, one
/// hyphenated word.
///
/// [`crate::Phase`] deliberately carries no spelling of its own — `state.rs` says the
/// lower-case forms belong to whoever is printing — and this module is what prints the
/// evidence layout, so the rendering lives here beside the file names it produces.
const fn phase_word(phase: Phase) -> &'static str {
    match phase {
        Phase::Goal => "goal",
        Phase::Scope => "scope",
        Phase::AcceptanceTests => "acceptance-tests",
        Phase::Implement => "implement",
        Phase::Red => "red",
        Phase::Green => "green",
        Phase::Refactor => "refactor",
        Phase::Review => "review",
        Phase::Harden => "harden",
        Phase::DoneCheck => "done-check",
        Phase::Verify => "verify",
        Phase::Publish => "publish",
    }
}

/// The setting that would have configured `kind`, named in the refusal that says it
/// holds no command.
///
/// [`profile_from`] is what maps these keys to gate kinds, and this mirrors its table
/// rather than deriving from it: `targeted_test_command` is the one key that does not
/// follow the `{kind}_command` shape, and the refusal has to name what an operator has
/// to go and set, not the shape it would have fitted.
fn gate_setting(kind: GateKind) -> String {
    if kind == GateKind::Targeted {
        return "targeted_test_command".to_owned();
    }
    format!("{kind}_command")
}

/// The summary a gate's run reported, read from whichever stream it wrote it on.
///
/// Standard error second, not merged into the first: `cargo test` writes its report to
/// standard output and a harness that is not cargo's writes its own wherever it likes,
/// and a run that reported its counts on the other stream is still reporting. Neither
/// stream holding a report is [`None`] rather than an empty summary — see
/// [`no_test_report`] for what reading that as zero failures would pass.
fn test_report(result: &GateResult) -> Option<TestSummary> {
    parse_cargo(&result.stdout).or_else(|| parse_cargo(&result.stderr))
}

/// What the checkout held at the instant the phase's gate ran.
///
/// §9 wants the tree hash filed with a phase's evidence, so that a later reader can say
/// *which* work a green verdict belongs to. It is a digest rather than a git object
/// name, and deliberately so: the tree the gate ran against is the working tree, which
/// git has no OID for — a green phase's edits are unstaged, and `HEAD` names the commit
/// the phase started from, which never moves here. So this digests the base the
/// preflight recorded together with the content digest of every path that differs from
/// it, which is the same measurement [`protocol::check_scope`] refuses over
/// ([`crate::git::changed_paths`], tracked edits and never-staged files together).
///
/// Two trees that differ by one byte of one changed file digest differently, and two
/// checkouts of one base whose changes are identical digest the same: the question §9
/// asks is "is this the work that was gated", not "which commit is this".
fn tree_hash(prep: &Prepared) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(TREE_PREFIX);
    digest.update(prep.base_sha.as_bytes());
    for path in git::changed_paths(&prep.worktree, &prep.base_sha)? {
        digest.update([0]);
        digest.update(path.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(content_state(&prep.worktree.join(&path))?.as_bytes());
    }
    Ok(hex(digest.finalize().as_ref()))
}

/// What one changed path held, as the one letter and digest that say it: its content, or
/// that it was gone. A path git listed as deleted from the base is read as absent rather
/// than refused, because its absence *is* the change.
fn content_state(path: &Path) -> Result<String> {
    match fs::read(path) {
        Ok(bytes) => Ok(format!("A{}", hex(Sha256::digest(&bytes).as_ref()))),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok("D".to_owned()),
        Err(why) => Err(why.into()),
    }
}

/// Lowercase hexadecimal, the way the project id and the failure signature spell it.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

/// What one phase's gate run left, gathered for the evidence that files it.
///
/// Three borrows rather than six arguments, so that the record can be built in one
/// place: what a phase filed and what its gate journalled have to agree, and a
/// [`PhaseRun`] that could hold half of a run is how they would stop agreeing.
struct PhaseRun<'a> {
    /// The gate the phase's declaration named, as this project configured it.
    gate: &'a Gate,
    /// What running it answered.
    result: &'a GateResult,
    /// The names the phase's verdict named — empty when it refused, because a refusal
    /// named nothing new.
    named: &'a [String],
}

/// One phase's evidence, as one line of `phases/<phase>.jsonl`.
///
/// The three things VISION.md §9 names — command, output, tree hash — and the three
/// that make them readable: which phase and which gate they belong to, and the verdict
/// the pair produced. The command is kept as the words it was spawned with rather than
/// joined into a string, because joining is a claim about quoting and this record has to
/// be re-runnable exactly.
#[derive(Serialize)]
struct PhaseEvidence<'a> {
    /// Which phase filed this line, in the word its file is named by.
    phase: &'a str,
    /// Which gate ran, which for §9's red and green is always the targeted one.
    gate: &'a str,
    /// The command the gate ran, as the separate words it was spawned with.
    command: &'a [String],
    /// The commit the phase's work was measured against, which is the base the
    /// preflight recorded rather than wherever `HEAD` stands.
    base_sha: &'a str,
    /// What the checkout held when the gate ran — see [`tree_hash`].
    tree_sha: String,
    /// The tests the phase's verdict named; empty for a phase it refused.
    names: &'a [String],
    /// The gate's own verdict on the run. Stored, not derived (ADR-0036).
    passed: bool,
    /// The status the command exited with, or `None` when it never produced one.
    exit_code: Option<i32>,
    /// Whether the run outlived its budget and was killed.
    timed_out: bool,
    /// Both streams verbatim. §9 stores the output rather than a summary of it, and a
    /// harness that reports on standard error is not reporting less.
    stdout: &'a str,
    /// The stream a command explains itself on, kept beside the other.
    stderr: &'a str,
}

/// Append one redacted line to a phase's evidence file, and flush it.
///
/// Appended rather than written, and synced rather than left in the page cache, for the
/// two reasons the attempt's own files are written the same way (ADR-0065): a phase that
/// gated twice has two answers worth reading, and evidence lost to a power cut is a run
/// nobody can review. The mode is set after the write as well as asked for at creation,
/// because a file that already existed keeps whatever mode somebody gave it.
fn append_private(path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PHASE_FILE_MODE)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    fs::set_permissions(path, Permissions::from_mode(PHASE_FILE_MODE))?;
    file.sync_all()?;
    Ok(())
}

/// Make `path` exist as a directory kept at [`PHASE_DIR_MODE`].
///
/// The same rule the attempt's other evidence levels follow: a link is refused rather
/// than written through, since writing through one would put a run's evidence wherever
/// the link points, and a level that is there and is not a directory is refused rather
/// than deleted to make room.
fn private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(seen) if seen.is_dir() => {}
        Ok(_) => return Err(occupied(path)),
        Err(why) if why.kind() == io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(why) => return Err(why.into()),
    }
    fs::set_permissions(path, Permissions::from_mode(PHASE_DIR_MODE))?;
    Ok(())
}

/// Refuse a level of the evidence layout that is there and is not a directory.
fn occupied(path: &Path) -> Error {
    Error::Policy {
        detail: format!(
            "`{}` is already there and is not a directory",
            path.display()
        ),
        paths: vec![path.to_path_buf()],
    }
}

/// Ask whether this project's repository lock can be taken, and give it back.
///
/// The lock is taken for real rather than inspected, because "the lock is
/// acquirable" is a claim about a link into a name somebody else may be holding
/// and only the attempt answers it. It is given back inside the same breath: what
/// VISION.md §6 requires is that the lock *can* be had, and a preflight that kept
/// it would block the run it was proving sane. A lock held by a live holder is
/// [`FailureClass::EnvironmentFailure`], the same class [`crate::classify()`] reads
/// off an [`crate::Error::Io`] — a machine two runs want at once is a machine that
/// is not ready, and the recovery the class chooses is to ask again later.
fn check_lock(state_dir: &Path) -> CheckOutcome {
    let path = lock::lock_path(state_dir);
    match lock::acquire(state_dir, Duration::ZERO) {
        Err(failure) => lock_refused(format!(
            "the repository lock at `{}` could not be taken: {failure}",
            path.display()
        )),
        Ok(held) => {
            let taken = held.path().to_path_buf();
            match held.release() {
                Ok(()) => CheckOutcome::Passed {
                    check: PreflightCheck::Lock,
                    detail: format!(
                        "the repository lock at `{}` was taken and given back",
                        taken.display()
                    ),
                },
                Err(failure) => lock_refused(format!(
                    "the repository lock at `{}` was taken but could not be given back: \
                     {failure}",
                    taken.display()
                )),
            }
        }
    }
}

/// A refusal of the lock check, whose class is always the machine's.
fn lock_refused(detail: String) -> CheckOutcome {
    CheckOutcome::Refused {
        check: PreflightCheck::Lock,
        class: FailureClass::EnvironmentFailure,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::{CheckOutcome, PreflightCheck, PreflightReport, check_disk, preflight};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        Bus, Capabilities, Config, Error, Event, EventKind, FailureClass, GateKind, Invocation,
        Journal, Outcome, Project, Provider, Result, lock,
    };
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    /// The program a gate fixture runs a script through, as `gate.rs`'s own
    /// fixtures do.
    const SHELL: &str = "/bin/sh";

    /// The script a passing baseline runs. It prints, so the evidence a check
    /// quotes has something in it.
    const GREEN: &str = "echo the baseline is green; exit 0";

    /// The script a refusing baseline runs: it says why on standard error, then
    /// exits non-zero.
    const BROKEN: &str = "echo the baseline is broken >&2; exit 1";

    /// The program no `PATH` holds, so a gate naming it cannot be started.
    const NO_PROGRAM: &str = "/this/program/is/not/here/nope";

    /// A worktree with its own origin and its own state directory, which is what
    /// [`preflight`] reads: a repository to fetch and judge, and a directory to
    /// write the rows it journals into.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
    }

    fn fixture() -> Fixture {
        let repo = scratch_repo().expect("a scratch repository is buildable");
        let id = "0123456789abcdef".to_owned();
        let state_dir = repo.path().join("state").join(&id);
        fs::create_dir_all(&state_dir).expect("a state directory is creatable");
        let work = repo.work().to_path_buf();
        Fixture {
            repo,
            project: Project {
                root: work,
                id,
                state_dir,
            },
        }
    }

    /// The configuration one preflight is given. `verify_command` is set because
    /// no profile at all can be built without it, and the disk floor is one byte
    /// so that a passing test says nothing about how full the machine running it
    /// happens to be.
    fn config(baseline: Option<&str>) -> Config {
        let mut settings = Config::default();
        settings.verify_command =
            Some(vec![SHELL.to_owned(), "-c".to_owned(), "exit 0".to_owned()]);
        settings.baseline_command =
            baseline.map(|script| vec![SHELL.to_owned(), "-c".to_owned(), script.to_owned()]);
        settings.min_free_disk_bytes = 1;
        settings
    }

    /// An adapter that answers every question about itself and counts how many
    /// sessions it was asked to run. The counter is the point: a preflight that
    /// probed availability by working would fail the test that reads it.
    struct Probe {
        name: &'static str,
        capabilities: Capabilities,
        sessions: Cell<u32>,
    }

    impl Probe {
        /// A probe answering for `name`, with every capability detected.
        fn named(name: &'static str) -> Self {
            Self {
                name,
                capabilities: Capabilities {
                    structured_output: true,
                    model_selection: true,
                    usage_telemetry: true,
                },
                sessions: Cell::new(0),
            }
        }

        /// A probe whose CLI was detected but answered no to all three.
        fn mute(name: &'static str) -> Self {
            Self {
                capabilities: Capabilities {
                    structured_output: false,
                    model_selection: false,
                    usage_telemetry: false,
                },
                ..Self::named(name)
            }
        }
    }

    impl Provider for Probe {
        fn name(&self) -> &str {
            self.name
        }

        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        fn invoke(&self, _inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
            self.sessions.set(self.sessions.get() + 1);
            Err(Error::Provider {
                provider: self.name.to_owned(),
                detail: "this probe runs no session".to_owned(),
            })
        }
    }

    /// One preflight of `fixture` that nothing in the fixture gives a reason to
    /// refuse, with a baseline that runs and passes.
    fn pass(fixture: &Fixture) -> PreflightReport {
        preflight(
            &fixture.project,
            &config(Some(GREEN)),
            &Probe::named("dummy"),
        )
        .expect("nothing here gives preflight a question it cannot ask")
    }

    /// Everything a project's journal holds, oldest first.
    fn rows(project: &Project) -> Vec<Event> {
        Journal::open_for(project)
            .expect("preflight opened this project's journal itself")
            .events()
            .expect("the journal preflight wrote is readable")
    }

    /// The journal's `kind` column for every row, oldest first.
    fn kinds(project: &Project) -> Vec<&'static str> {
        rows(project)
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// The check that stopped a report, with the test's certainty that there was
    /// one.
    fn refusal(report: &PreflightReport) -> &CheckOutcome {
        report
            .refusal()
            .expect("this report was expected to hold a refusal")
    }

    #[test]
    fn a_passing_preflight_records_each_check_in_the_order_it_asked_them() {
        let fixture = fixture();
        let report = pass(&fixture);

        assert!(report.passed());
        assert_eq!(report.refusal(), None);
        let asked: Vec<&str> = report
            .checks
            .iter()
            .map(|outcome| outcome.check().as_str())
            .collect();
        assert_eq!(
            asked,
            ["provider", "disk", "mainline", "baseline", "lock"],
            "the five checks are the five VISION.md §6 names, cheapest first"
        );
    }

    #[test]
    fn a_passing_preflight_names_the_fetched_tip_as_the_base() {
        let fixture = fixture();
        let report = pass(&fixture);

        assert_eq!(report.base_sha, fixture.repo.seed_sha());
    }

    #[test]
    fn a_passing_preflight_starts_no_provider_session() {
        let fixture = fixture();
        let probe = Probe::named("dummy");

        let report = preflight(&fixture.project, &config(Some(GREEN)), &probe)
            .expect("a probe that answers for the configured provider is an answer");

        assert!(report.passed());
        assert_eq!(
            probe.sessions.get(),
            0,
            "preflight proves the world sane without spending a token"
        );
    }

    #[test]
    fn a_passing_preflight_journals_the_gate_and_nothing_else() {
        let fixture = fixture();
        let report = pass(&fixture);

        assert_eq!(
            kinds(&fixture.project),
            ["GateStarted", "GateFinished"],
            "the one check that runs a command journals the pair that runs it"
        );
        let written = rows(&fixture.project);
        assert!(
            written.iter().all(|row| row.task_id.is_none()),
            "no task is running yet, so no row names one"
        );
        let EventKind::GateFinished { result } = &written[1].kind else {
            panic!("the second row is a gate finishing: {:?}", written[1].kind);
        };
        assert_eq!(result.kind, GateKind::Baseline);
        assert!(result.passed, "{}", report.checks[3].detail());
    }

    #[test]
    fn a_baseline_nobody_configured_passes_naming_the_key_that_would_set_one() {
        let fixture = fixture();
        let report = preflight(&fixture.project, &config(None), &Probe::named("dummy"))
            .expect("an unconfigured baseline is an answer, not a failure to ask");

        assert!(report.passed());
        let baseline = &report.checks[3];
        assert_eq!(baseline.check(), PreflightCheck::Baseline);
        assert!(
            baseline.detail().contains("baseline_command"),
            "a pass must name the key an operator would set: {}",
            baseline.detail()
        );
        assert!(
            kinds(&fixture.project).is_empty(),
            "a gate nobody configured is never run: {:?}",
            kinds(&fixture.project)
        );
    }

    #[test]
    fn the_provider_check_carries_what_the_adapter_answered_about_itself() {
        let fixture = fixture();
        let report = pass(&fixture);

        let detail = report.checks[0].detail();
        assert!(detail.contains("dummy"), "{detail}");
        assert!(
            detail.contains("structured output, model selection, usage telemetry"),
            "{detail}"
        );
    }

    #[test]
    fn an_adapter_that_reports_no_capabilities_passes_saying_so() {
        let fixture = fixture();
        let probe = Probe::mute("dummy");

        let report = preflight(&fixture.project, &config(Some(GREEN)), &probe)
            .expect("an adapter with no capabilities is still the right adapter");

        assert_eq!(report.checks[0].check(), PreflightCheck::Provider);
        assert!(report.checks[0].passed());
        assert!(
            report.checks[0]
                .detail()
                .contains("nothing beyond a prompt in and text out"),
            "{}",
            report.checks[0].detail()
        );
    }

    #[test]
    fn an_adapter_answering_for_another_cli_is_refused_as_a_provider_configuration_failure() {
        let fixture = fixture();
        let report = preflight(
            &fixture.project,
            &config(Some(GREEN)),
            &Probe::named("codex"),
        )
        .expect("a refusal is a report, not an error");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Provider);
        assert_eq!(stopped.class(), Some(FailureClass::ProviderConfiguration));
        assert!(stopped.detail().contains("codex"), "{stopped}");
        assert!(stopped.detail().contains("dummy"), "{stopped}");
        assert_eq!(
            report.checks.len(),
            1,
            "the first refusal ends the checks: {}",
            report.evidence()
        );
    }

    #[test]
    fn a_remote_that_cannot_be_fetched_is_refused_as_a_git_conflict() {
        let fixture = fixture();
        let mut settings = config(Some(GREEN));
        settings.mainline_remote = "/no/such/remote/at/all".to_owned();

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("an unreachable remote is an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Mainline);
        assert_eq!(stopped.class(), Some(FailureClass::GitConflict));
        assert!(
            stopped.detail().contains("/no/such/remote/at/all"),
            "{stopped}"
        );
        assert_eq!(report.base_sha, "", "no fetch proved a tip to base on");
        assert_eq!(report.checks.len(), 3, "{}", report.evidence());
    }

    #[test]
    fn a_mainline_the_remote_has_never_held_is_refused_as_a_git_conflict() {
        let fixture = fixture();
        let mut settings = config(Some(GREEN));
        settings.mainline_branch = "never-pushed".to_owned();

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("a remote holding no such branch is an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Mainline);
        assert_eq!(stopped.class(), Some(FailureClass::GitConflict));
        assert!(stopped.detail().contains("never-pushed"), "{stopped}");
        assert_eq!(report.checks.len(), 3, "{}", report.evidence());
    }

    #[test]
    fn an_unclean_checkout_is_refused_as_a_policy_failure_naming_the_file() {
        let fixture = fixture();
        fs::write(fixture.repo.work().join("loose.rs"), "uncommitted\n")
            .expect("a file is writable in the work tree");

        let report = preflight(
            &fixture.project,
            &config(Some(GREEN)),
            &Probe::named("dummy"),
        )
        .expect("a dirty tree is an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Mainline);
        assert_eq!(stopped.class(), Some(FailureClass::PolicyFailure));
        assert!(stopped.detail().contains("loose.rs"), "{stopped}");
        assert_eq!(
            report.base_sha, "",
            "a mainline check that refused names no base for work that may not start"
        );
    }

    #[test]
    fn a_filesystem_below_the_floor_is_refused_as_an_environment_failure() {
        let fixture = fixture();
        let mut settings = config(Some(GREEN));
        settings.min_free_disk_bytes = u64::MAX;

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("a full disk is an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::DiskSpace);
        assert_eq!(stopped.class(), Some(FailureClass::EnvironmentFailure));
        assert!(
            stopped.detail().contains(&u64::MAX.to_string(),),
            "the refusal names the floor it could not clear: {stopped}"
        );
        assert_eq!(report.checks.len(), 2, "{}", report.evidence());
    }

    #[test]
    fn a_filesystem_that_cannot_be_asked_is_refused_as_an_environment_failure() {
        let outcome = check_disk(Path::new("/no/such/directory/for/statvfs"), 1);

        assert!(!outcome.passed());
        assert_eq!(outcome.check(), PreflightCheck::DiskSpace);
        assert_eq!(outcome.class(), Some(FailureClass::EnvironmentFailure));
        assert!(
            outcome.detail().contains("/no/such/directory/for/statvfs"),
            "{}",
            outcome.detail()
        );
    }

    #[test]
    fn a_refusing_baseline_gate_is_refused_as_a_verification_failure() {
        let fixture = fixture();
        let report = preflight(
            &fixture.project,
            &config(Some(BROKEN)),
            &Probe::named("dummy"),
        )
        .expect("a gate that ran and refused is an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Baseline);
        assert_eq!(stopped.class(), Some(FailureClass::VerificationFailure));
        assert!(stopped.detail().contains("exit 1"), "{stopped}");
        assert!(stopped.detail().contains("code 1"), "{stopped}");
        assert!(
            stopped.detail().contains("the baseline is broken"),
            "a refusal carries the gate's own words: {stopped}"
        );
    }

    #[test]
    fn a_baseline_gate_that_outlives_its_budget_is_an_environment_failure() {
        let fixture = fixture();
        let mut settings = config(Some("sleep 30"));
        settings.gate_timeout_secs = 1;

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("a gate that ran out of time is an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Baseline);
        assert_eq!(stopped.class(), Some(FailureClass::EnvironmentFailure));
        assert!(stopped.detail().contains("1 s budget"), "{stopped}");
        let written = rows(&fixture.project);
        let EventKind::GateFinished { result } = &written[1].kind else {
            panic!("the gate finished, by being killed: {:?}", written[1].kind);
        };
        assert!(
            result.timed_out,
            "the journal says a timeout, not a failure"
        );
    }

    #[test]
    fn a_baseline_gate_that_cannot_be_started_leaves_a_start_and_no_finish() {
        let fixture = fixture();
        let mut settings = config(None);
        settings.baseline_command = Some(vec![NO_PROGRAM.to_owned()]);

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("a gate that cannot start is still an answer");

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Baseline);
        assert_eq!(stopped.class(), Some(FailureClass::EnvironmentFailure));
        assert!(stopped.detail().contains(NO_PROGRAM), "{stopped}");
        assert_eq!(report.checks.len(), 4, "{}", report.evidence());
        assert_eq!(
            kinds(&fixture.project),
            ["GateStarted"],
            "a gate that never ran has the row that says it never finished"
        );
    }

    #[test]
    fn a_held_repository_lock_is_refused_as_an_environment_failure() {
        let fixture = fixture();
        let held = lock::acquire(&fixture.project.state_dir, Duration::ZERO)
            .expect("the lock is free for the fixture to take");

        let report = pass(&fixture);

        let stopped = refusal(&report);
        assert_eq!(stopped.check(), PreflightCheck::Lock);
        assert_eq!(stopped.class(), Some(FailureClass::EnvironmentFailure));
        assert!(
            stopped.detail().contains(
                &lock::lock_path(&fixture.project.state_dir)
                    .display()
                    .to_string()
            ),
            "the refusal names the lock file: {stopped}"
        );
        assert_eq!(report.checks.len(), 5, "{}", report.evidence());
        held.release().expect("the fixture gives its lock back");
    }

    #[test]
    fn a_free_lock_is_taken_and_given_back() {
        let fixture = fixture();
        let report = pass(&fixture);

        let lock_line = &report.checks[4];
        assert_eq!(lock_line.check(), PreflightCheck::Lock);
        assert!(lock_line.passed(), "{lock_line}");
        assert!(lock_line.detail().contains("given back"), "{lock_line}");
        assert!(
            !lock::lock_path(&fixture.project.state_dir).exists(),
            "the check asks whether the lock can be taken; it does not keep it"
        );
    }

    #[test]
    fn the_first_refusal_stops_the_checks_behind_it() {
        let fixture = fixture();
        let mut settings = config(Some(BROKEN));
        settings.min_free_disk_bytes = u64::MAX;

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("a full disk is answered without running a gate");

        assert_eq!(report.checks.len(), 2, "{}", report.evidence());
        assert_eq!(report.checks[1].check(), PreflightCheck::DiskSpace);
        assert!(
            kinds(&fixture.project).is_empty(),
            "a stopped preflight runs no gate: {:?}",
            kinds(&fixture.project)
        );
    }

    #[test]
    fn a_passed_report_asks_for_the_event_that_records_the_base() {
        let fixture = fixture();
        let report = pass(&fixture);

        assert_eq!(
            report.event(),
            EventKind::PreflightPassed {
                base_sha: fixture.repo.seed_sha().to_owned(),
            }
        );
    }

    #[test]
    fn a_refused_report_asks_for_one_failure_event_holding_every_line_it_has() {
        let fixture = fixture();
        let mut settings = config(Some(GREEN));
        settings.min_free_disk_bytes = u64::MAX;

        let report = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect("a refusal is a report");

        let EventKind::PreflightFailed { class, detail } = report.event() else {
            panic!(
                "a refused report answers with a failure: {:?}",
                report.event()
            );
        };
        assert_eq!(class, FailureClass::EnvironmentFailure);
        assert_eq!(detail, report.evidence());
        assert!(detail.contains("provider: passed"), "{detail}");
        assert!(detail.contains("disk: refused"), "{detail}");
        assert!(
            !detail.contains("baseline"),
            "a check that never ran has no line: {detail}"
        );
    }

    #[test]
    fn a_project_with_no_state_directory_cannot_be_asked_at_all() {
        let repo = scratch_repo().expect("a scratch repository is buildable");
        let project = Project {
            root: repo.work().to_path_buf(),
            id: "0123456789abcdef".to_owned(),
            state_dir: repo.path().join("never-registered"),
        };

        let error = preflight(&project, &config(Some(GREEN)), &Probe::named("dummy"))
            .expect_err("preflight writes to a directory registration owns");

        assert!(matches!(error, Error::Database(_)), "{error}");
        assert!(
            !repo.path().join("never-registered").exists(),
            "preflight never conjures a state directory"
        );
    }

    #[test]
    fn a_configuration_that_cannot_build_a_profile_is_an_error_and_not_a_refusal() {
        let fixture = fixture();
        let mut settings = Config::default();
        settings.min_free_disk_bytes = 1;

        let error = preflight(&fixture.project, &settings, &Probe::named("dummy"))
            .expect_err("a configuration with no mandatory gate cannot be run at all");

        let Error::Config { key, detail } = error else {
            panic!("the mandatory-gate rule is a config error: {error}");
        };
        assert_eq!(key, "verify_command");
        assert!(detail.contains("mandatory"), "{detail}");
        assert!(
            kinds(&fixture.project).is_empty(),
            "an unbuildable profile journals nothing: {:?}",
            kinds(&fixture.project)
        );
    }

    #[test]
    fn a_check_reads_as_one_line_and_a_report_as_one_line_per_check() {
        let passed = CheckOutcome::Passed {
            check: PreflightCheck::Baseline,
            detail: "the baseline is green".to_owned(),
        };
        let refused = CheckOutcome::Refused {
            check: PreflightCheck::DiskSpace,
            class: FailureClass::EnvironmentFailure,
            detail: "1 byte free".to_owned(),
        };

        assert_eq!(
            passed.to_string(),
            "baseline: passed — the baseline is green"
        );
        assert_eq!(passed.class(), None);
        assert_eq!(passed.detail(), "the baseline is green");
        assert_eq!(
            refused.to_string(),
            "disk: refused (EnvironmentFailure) — 1 byte free"
        );
        assert_eq!(refused.check(), PreflightCheck::DiskSpace);

        let report = PreflightReport {
            checks: vec![passed, refused],
            base_sha: String::new(),
        };
        assert!(!report.passed());
        assert_eq!(refusal(&report).check(), PreflightCheck::DiskSpace);
        assert_eq!(
            report.evidence(),
            "baseline: passed — the baseline is green\n\
             disk: refused (EnvironmentFailure) — 1 byte free"
        );
        assert_eq!(PreflightCheck::Mainline.to_string(), "mainline");
    }
}

#[cfg(test)]
mod new {
    //! The construction of a [`Runner`] and the one attempt it opens.
    //!
    //! The module is named after the call it tests because the task that asked
    //! for the runner fixed `test(/runner::new/)` as its Verify command, and a
    //! module named `tests` would make that command select nothing.
    //! `journal.rs` names its test modules the same way (`replay`, `streaming`,
    //! `projection`), so this is the house shape, not an exception made for one
    //! filter.

    use super::Runner;
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Error, Event, EventKind, Journal, Project, Task, TaskId, evidence_dir,
        journal_path, parse_plan, project_config_path, read_evidence,
    };
    use std::fs;

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The settings a run needs in order to be constructible at all.
    ///
    /// Both keys are load-bearing. `verify_command` is mandatory: no profile can
    /// be built without it, so a fixture that omitted it would be testing the
    /// refusal rather than a run. `provider` names `claude` rather than leaving
    /// the configured default `dummy` because the `dummy` adapter replays a
    /// scenario file and refuses to be built without one — so a run that opens on
    /// these settings is proof that the project's own word reached the adapter,
    /// which the default word could not have done.
    const BASE: &str =
        "provider = \"claude\"\nverify_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n";

    /// `BASE` with `extra` written below it, one settings key per line.
    fn settings(extra: &[&str]) -> String {
        let mut document = BASE.to_owned();
        for line in extra {
            document.push_str(line);
            document.push('\n');
        }
        document
    }

    /// A registered project: a repository of its own, a state directory outside
    /// the worktree, and one settings document saying what a run here is for.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
    }

    impl Fixture {
        /// A project whose own settings document holds `document` verbatim.
        fn with_settings(document: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::write(project_config_path(&project), document)
                .expect("a project settings document is writable");
            Self { repo, project }
        }

        /// The commit a preflight would have handed the run: the one the scratch
        /// origin holds and the worktree was cut from.
        fn base(&self) -> String {
            self.repo.seed_sha().to_owned()
        }
    }

    /// Every row the journal holds, read on a second connection: the way an
    /// operator, or a process that came after the run, reads it.
    fn rows(project: &Project) -> Vec<Event> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events()
            .expect("a journal of rows is readable")
    }

    /// The rows that opened an attempt, which is what "exactly one event" is
    /// counted over.
    fn started(project: &Project) -> Vec<Event> {
        rows(project)
            .into_iter()
            .filter(|row| row.kind.discriminant() == "AttemptStarted")
            .collect()
    }

    /// A queue row worked under `protocol` when it names one, and under nothing
    /// when `None` leaves the project's word to answer.
    fn task(protocol: Option<&str>) -> Task {
        let mut document = "\
## T088 Runner scaffolding and the attempt record

**Outcome:** the runner type exists and can open one attempt.
**Done-when:** one attempt journals exactly one row.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::new/)'`
**Refs:** VISION.md section 6
"
        .to_owned();
        if let Some(word) = protocol {
            document.push_str("**Protocol:** ");
            document.push_str(word);
            document.push('\n');
        }
        let mut queue = parse_plan(&document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        queue.remove(0)
    }

    /// Journal what preflight journals, so an attempt has a base to start from.
    ///
    /// It is written by the run's own recorder because that is who writes a
    /// preflight verdict in the lifecycle; a base handed in through some other
    /// door is a shape a run never meets.
    fn give_base(run: &mut Runner, task: TaskId, base_sha: &str) {
        run.recorder
            .record(
                Some(task),
                EventKind::PreflightPassed {
                    base_sha: base_sha.to_owned(),
                },
            )
            .expect("a preflight verdict is journalable");
    }

    /// The four facts of one `AttemptStarted` row, refused for any other kind.
    fn facts(event: &Event) -> (AttemptId, String, u32, String) {
        match &event.kind {
            EventKind::AttemptStarted {
                attempt,
                protocol,
                pid,
                base_sha,
            } => (*attempt, protocol.clone(), *pid, base_sha.clone()),
            other => panic!(
                "expected the facts of an attempt, got a row of kind `{}`",
                other.discriminant()
            ),
        }
    }

    #[test]
    fn a_run_is_opened_from_a_registered_project_and_nothing_else() {
        let fixture = Fixture::with_settings(BASE);

        let run = Runner::new(fixture.project.clone())
            .expect("a registered project, and nothing besides it, opens a run");

        assert_eq!(
            run.project, fixture.project,
            "a run works the project it was handed"
        );
        assert!(
            journal_path(&fixture.project.state_dir).is_file(),
            "opening a run opens the journal its transitions are persisted into"
        );
        let shown = format!("{run:?}");
        assert!(
            shown.starts_with(&format!("Runner {{ project: {PROJECT_ID:?}")),
            "a run says what it is and which project it is working: {shown}"
        );
        assert!(
            shown.contains(&format!("project: {PROJECT_ID:?}")),
            "and names the project by its registered id: {shown}"
        );
    }

    #[test]
    fn a_run_works_with_the_adapter_its_project_named() {
        let fixture = Fixture::with_settings(BASE);

        let run = Runner::new(fixture.project.clone())
            .expect("a project naming an adapter this build has is a runnable project");

        let shown = format!("{run:?}");
        assert!(
            shown.contains("configured: \"claude\""),
            "the project's own word is what the run was configured with: {shown}"
        );
        assert!(
            shown.contains("adapter: \"claude\""),
            "and the adapter built from it is the claude adapter: {shown}"
        );
        assert!(
            !shown.contains("dummy"),
            "the configured default is not left standing where the project said \
             something else: {shown}"
        );
    }

    #[test]
    fn a_run_holds_the_gates_its_project_configured_in_the_order_a_run_runs_them() {
        let fixture = Fixture::with_settings(&settings(&[
            "build_command = [\"/bin/sh\", \"-c\", \"exit 0\"]",
            "lint_command = [\"/bin/sh\", \"-c\", \"exit 0\"]",
        ]));

        let run = Runner::new(fixture.project.clone())
            .expect("a project configuring its gates is a runnable project");

        let shown = format!("{run:?}");
        assert!(
            shown.contains("gates: [\"verify\", \"lint\", \"build\"]"),
            "the three configured gates in the order a run executes them, not the \
             order the document wrote them in: {shown}"
        );
        assert!(
            !shown.contains("flake"),
            "a gate nobody configured is not in the profile: {shown}"
        );
    }

    #[test]
    fn a_project_that_configured_no_verify_command_refuses_the_run() {
        let fixture = Fixture::with_settings(
            "provider = \"claude\"\nlint_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n",
        );

        let refused = Runner::new(fixture.project.clone())
            .expect_err("a project with no complete local suite cannot be run");

        match refused {
            Error::Config { key, detail } => {
                assert_eq!(
                    key, "verify_command",
                    "the refusal names the key to write: {detail}"
                );
                assert!(
                    detail.contains("mandatory"),
                    "and the rule it refused to bend: {detail}"
                );
            }
            other => panic!("a missing gate is a configuration refusal, got: {other}"),
        }
        assert!(
            !journal_path(&fixture.project.state_dir).is_file(),
            "a run that was refused before it began opened no journal"
        );
    }

    #[test]
    fn a_project_that_named_an_adapter_nobody_has_refuses_the_run() {
        let fixture = Fixture::with_settings(
            "provider = \"claude-code\"\nverify_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n",
        );

        let refused = Runner::new(fixture.project.clone())
            .expect_err("a project naming an adapter this build lacks cannot be run");

        match refused {
            Error::Config { key, detail } => {
                assert_eq!(
                    key, "provider",
                    "the refusal names the key to fix: {detail}"
                );
                assert!(
                    detail.contains("claude-code"),
                    "and quotes the word it could not honour: {detail}"
                );
            }
            other => panic!("an unknown adapter is a configuration refusal, got: {other}"),
        }
        assert!(
            !journal_path(&fixture.project.state_dir).is_file(),
            "a run that was refused before it began opened no journal"
        );
    }

    #[test]
    fn opening_an_attempt_adds_one_row_naming_the_process_and_the_base() {
        let fixture = Fixture::with_settings(BASE);
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, work.id, &fixture.base());
        let moved = fixture
            .repo
            .commit("late.md", "a commit no preflight ever proved")
            .expect("a scratch commit is committable");
        let before = rows(&fixture.project).len();

        let attempt = run
            .begin_attempt(&work)
            .expect("a task preflight gave a base to can be attempted");

        assert_eq!(
            attempt,
            AttemptId::new(1),
            "the first attempt of a task is number 1"
        );
        assert_eq!(
            rows(&fixture.project).len(),
            before + 1,
            "opening an attempt writes exactly one row and no other"
        );
        let opened = started(&fixture.project);
        assert_eq!(opened.len(), 1, "one row of kind `AttemptStarted`");
        let row = &opened[0];
        assert_eq!(row.task_id, Some(work.id), "the row belongs to the task");
        let (number, protocol, pid, base_sha) = facts(row);
        assert_eq!(number, AttemptId::new(1));
        assert_eq!(
            protocol, "direct",
            "a task naming no protocol and a project naming none is worked direct"
        );
        assert_eq!(
            pid,
            std::process::id(),
            "the pid is the process recovery would go looking for"
        );
        assert_eq!(
            base_sha,
            fixture.base(),
            "the base is the commit preflight recorded, not wherever HEAD happens to be"
        );
        assert_ne!(
            base_sha, moved,
            "and a commit that appeared after preflight is not the base of an attempt"
        );
    }

    #[test]
    fn a_view_of_the_run_is_told_what_an_attempt_recorded() {
        let fixture = Fixture::with_settings(BASE);
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, work.id, &fixture.base());

        let mut view = run.subscribe();
        run.begin_attempt(&work)
            .expect("a task with a base can be attempted");

        let (seen, lost) = view.drain();
        assert_eq!(seen.len(), 1, "the view was told one event");
        assert_eq!(lost, 0, "and lost none of it");
        assert_eq!(
            seen[0].kind.discriminant(),
            "AttemptStarted",
            "the view follows the run it was opened on"
        );
        assert_eq!(seen[0].task_id, Some(work.id));
    }

    #[test]
    fn an_attempt_is_worked_under_the_protocol_its_task_named() {
        let fixture = Fixture::with_settings(&settings(&["default_protocol = \"direct\""]));
        let work = task(Some("tdd"));
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, work.id, &fixture.base());

        run.begin_attempt(&work)
            .expect("a task naming a protocol this build runs can be attempted");

        let opened = started(&fixture.project);
        let (_, protocol, _, _) = facts(&opened[0]);
        assert_eq!(
            protocol, "tdd",
            "a task's own word outranks the project's, which here said otherwise"
        );
    }

    #[test]
    fn an_attempt_with_no_word_of_its_own_uses_the_protocol_its_project_named() {
        let fixture = Fixture::with_settings(&settings(&["default_protocol = \"tdd\""]));
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, work.id, &fixture.base());

        run.begin_attempt(&work)
            .expect("a task that names no protocol is worked under the project's");

        let opened = started(&fixture.project);
        let (_, protocol, _, _) = facts(&opened[0]);
        assert_eq!(
            protocol, "tdd",
            "the project's word answers when the task's is absent"
        );
    }

    #[test]
    fn an_attempt_has_evidence_from_the_moment_it_starts() {
        let fixture = Fixture::with_settings(&settings(&["model = \"the-configured-model\""]));
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, work.id, &fixture.base());

        let attempt = run
            .begin_attempt(&work)
            .expect("a task with a base can be attempted");

        let filed = read_evidence(&fixture.project, work.id)
            .expect("an evidence directory that is not there reads back empty, not broken");
        assert_eq!(filed.len(), 1, "starting an attempt files its record");
        let record = &filed[0];
        assert_eq!(record.id, attempt, "filed under the attempt that opened");
        assert_eq!(record.task, work.id, "and under the task that ran");
        assert_eq!(
            record.base_sha,
            fixture.base(),
            "naming the tree it started from"
        );
        assert_eq!(
            record.exit_reason,
            format!("started as pid {}", std::process::id()),
            "saying what the attempt is doing, since it has not stopped"
        );
        assert!(
            record.ended.is_none(),
            "an attempt that has not stopped reports no end"
        );
        assert!(
            record.gates.is_empty(),
            "no gate has run yet, and an empty list is the evidence of that"
        );
        assert!(
            record.usage.is_none(),
            "there was no session to report a cost"
        );
        assert!(
            record.model_configured.as_deref() == Some("the-configured-model"),
            "the model the configuration asked for is known before the session starts"
        );
        assert!(
            record.model_reported.is_none(),
            "nothing has been reported yet"
        );
        assert!(record.session_id.is_none(), "no session has been opened");
        assert!(
            record.candidate_sha.is_none(),
            "an attempt that only started has produced no commit"
        );

        let dir = evidence_dir(&fixture.project, work.id, attempt);
        assert_eq!(
            fs::read_to_string(dir.join("context.md")).expect("the context file is there"),
            String::new(),
            "an attempt starts knowing nothing: the context is assembled after it"
        );
        for artifact in ["record.json", "report.md", "context.md"] {
            assert!(
                dir.join(artifact).is_file(),
                "an attempt's evidence directory holds `{artifact}` from its first moment"
            );
        }
        assert!(
            rows(&fixture.project)
                .iter()
                .all(|row| row.kind.discriminant() != "AttemptRecorded"),
            "filing evidence at the start is a file, not a second journal row"
        );
    }

    #[test]
    fn an_attempt_for_a_task_no_preflight_gave_a_base_is_refused() {
        let fixture = Fixture::with_settings(BASE);
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");

        let refused = run
            .begin_attempt(&work)
            .expect_err("an attempt needs the base preflight established");

        match refused {
            Error::NotFound { what } => assert!(
                what.contains(&work.id.to_string()),
                "the refusal names the task that has no base: {what}"
            ),
            other => panic!("an absent base is a not-found, got: {other}"),
        }
        assert!(
            rows(&fixture.project).is_empty(),
            "a refused attempt is not a transition, so nothing was journaled"
        );
        assert!(
            read_evidence(&fixture.project, work.id)
                .expect("no evidence directory reads back empty")
                .is_empty(),
            "and no evidence was filed for an attempt that never started"
        );
    }

    #[test]
    fn a_base_journaled_for_another_task_is_not_a_base() {
        let fixture = Fixture::with_settings(BASE);
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, TaskId::new(7), &fixture.base());

        let refused = run
            .begin_attempt(&work)
            .expect_err("another task's base says nothing about this one");

        assert!(
            matches!(refused, Error::NotFound { .. }),
            "a base belongs to the task preflight checked: {refused}"
        );
        assert!(
            started(&fixture.project).is_empty(),
            "and it opens no attempt"
        );
    }

    #[test]
    fn each_attempt_a_task_makes_numbers_itself_after_the_last() {
        let fixture = Fixture::with_settings(BASE);
        let work = task(None);
        let mut run =
            Runner::new(fixture.project.clone()).expect("the fixture project is runnable");
        give_base(&mut run, work.id, &fixture.base());

        let first = run
            .begin_attempt(&work)
            .expect("a task with a base can be attempted");
        let second = run
            .begin_attempt(&work)
            .expect("a task can be attempted again after its first attempt");
        assert_eq!(first, AttemptId::new(1));
        assert_eq!(
            second,
            AttemptId::new(2),
            "a retry is a new attempt of its own"
        );
        drop(run);

        let mut reopened = Runner::new(fixture.project.clone())
            .expect("a project with a journal is still a runnable project");
        let third = reopened
            .begin_attempt(&work)
            .expect("a supervisor that started again can attempt the task again");

        assert_eq!(
            third,
            AttemptId::new(3),
            "numbering comes from what was journaled, so a restart continues it"
        );
        assert_eq!(started(&fixture.project).len(), 3, "one row per attempt");
        let filed = read_evidence(&fixture.project, work.id)
            .expect("each attempt files evidence of its own");
        assert_eq!(filed.len(), 3, "three attempts, three records");
        let numbers: Vec<u32> = filed.iter().map(|record| record.id.get()).collect();
        assert_eq!(numbers, vec![1, 2, 3], "each numbered once, in order");
    }
}

#[cfg(test)]
mod prepare {
    //! The step between `queued` and `running`: the checks, the verdict they earn,
    //! the lock that is then taken, and the checkout an attempt is worked in.
    //!
    //! Named after the method it tests, the way `mod new` is named after `new`,
    //! because the task that asked for this step fixed `test(/runner::prepare/)` as
    //! its Verify command.

    use super::{Prepared, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        EventKind, FailureClass, Journal, Project, Task, TaskId, git, lock, parse_plan,
        project_config_path,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The settings a run is buildable from and a preflight has no reason to refuse.
    ///
    /// `verify_command` is mandatory (VISION.md §8) and `provider` names an adapter
    /// this build has, so every run under test here is an ordinary one. The disk
    /// floor is one byte: a test that cleared the 2 GiB default would be reporting
    /// how full the machine running it happens to be. No `baseline_command` is set,
    /// because the baseline check answers that as "nothing configured, nothing to
    /// prove" — these tests are about the verdict, the lock and the checkout, and
    /// the gate itself is [`crate::gate`]'s own coverage.
    const BASE: &str = concat!(
        "provider = \"claude\"\n",
        "verify_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n",
        "min_free_disk_bytes = 1\n",
    );

    /// A registered project: a repository of its own to fetch and to cut a
    /// checkout out of, and a state directory of its own to hold the lock.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
    }

    impl Fixture {
        /// A project holding `BASE` as its own settings document.
        fn new() -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::write(project_config_path(&project), BASE)
                .expect("a project settings document is writable");
            Self { repo, project }
        }

        /// The run this project is configured to have.
        fn run(&self) -> Runner {
            Runner::new(self.project.clone()).expect("a registered, configured project opens a run")
        }

        /// The file a run holds when it holds the repository lock.
        fn lock_file(&self) -> PathBuf {
            lock::lock_path(&self.project.state_dir)
        }

        /// Every checkout the repository has registered, the user's included.
        fn checkouts(&self) -> Vec<git::Worktree> {
            git::list_worktrees(&self.project.root).expect("the repository answers what it holds")
        }
    }

    /// The queue's one task: a block with the four mandatory sections.
    fn task() -> Task {
        let document = "\
## T001 Runner step: preflight and worktree

**Outcome:** a task reaches the point where an agent could start.
**Done-when:** the lock is held and the checkout is the fetched commit.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::prepare/)'`
**Refs:** VISION.md sections 6 and 10
";
        parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan")
            .remove(0)
    }

    /// The kinds the journal holds for `work`, oldest first.
    ///
    /// Read on a second connection, because that is who asks this question in real
    /// life: the TUI, `ktask-rs status`, and the process that comes after a run that
    /// died.
    fn kinds(project: &Project, work: TaskId) -> Vec<&'static str> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(work)
            .expect("the rows this run wrote are readable")
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// The base the `PreflightPassed` row recorded, or a failing test.
    fn recorded_base(project: &Project, work: TaskId) -> String {
        for row in Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(work)
            .expect("the rows this run wrote are readable")
        {
            if let EventKind::PreflightPassed { base_sha } = row.kind {
                return base_sha;
            }
        }
        panic!("a passing preflight records the base the work is cut from");
    }

    /// The class and evidence of the `PreflightFailed` row a refusal left.
    fn refusal(project: &Project, work: TaskId) -> (FailureClass, String) {
        for row in Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(work)
            .expect("the rows this run wrote are readable")
        {
            if let EventKind::PreflightFailed { class, detail } = row.kind {
                return (class, detail);
            }
        }
        panic!("a refused preflight leaves the row that ends the task");
    }

    /// The commit `checkout` stands at.
    fn head_at(checkout: &Path) -> String {
        git::head_sha(checkout).expect("a task checkout answers where it stands")
    }

    /// Take the project's lock from a test, as an outsider would, and give it back.
    fn lock_is_free(project: &Project) -> bool {
        let taken = lock::acquire(&project.state_dir, Duration::ZERO);
        match taken {
            Ok(held) => {
                held.release().expect("a lock this test took is given back");
                true
            }
            Err(_) => false,
        }
    }

    /// Ask `run` to prepare the queue's task, and insist it answers.
    fn prepared(run: &mut Runner) -> Prepared {
        run.prepare(&task())
            .expect("nothing in this fixture gives preflight a reason to refuse")
    }

    #[test]
    fn a_prepared_task_journals_the_start_then_the_verdict_and_nothing_else() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let work = task();

        let ready = run
            .prepare(&work)
            .expect("a clean fetched project prepares");

        assert_eq!(
            kinds(&fixture.project, work.id),
            ["PreflightStarted", "PreflightPassed"],
            "the start is journaled before the checks are asked and the verdict after \
             them; opening an attempt is the step after this one, and an agent session \
             after that"
        );
        assert_eq!(
            recorded_base(&fixture.project, work.id),
            fixture.repo.seed_sha(),
            "the base the row names is the tip of the fetched mainline"
        );
        assert_eq!(
            recorded_base(&fixture.project, work.id),
            ready.base_sha,
            "what the run hands the attempt is the base the journal recorded, not a \
             second opinion of it"
        );
    }

    #[test]
    fn the_checkout_is_cut_from_the_fetched_tip_and_not_from_where_head_stands() {
        let fixture = Fixture::new();
        let drift = fixture.repo.diverge("main").expect(
            "the working repository and its origin can each hold a commit the \
                      other has never seen",
        );
        let mut run = fixture.run();

        let ready = prepared(&mut run);

        assert_eq!(
            ready.base_sha, drift.remote,
            "the base is `<remote>/<branch>`'s tip after the fetch, which is the commit \
             every later gate measures the candidate against"
        );
        assert_eq!(
            head_at(&ready.worktree),
            drift.remote,
            "the checkout stands on the commit the fetch brought back"
        );
        assert_ne!(
            head_at(&ready.worktree),
            drift.local,
            "and not on wherever the user's checkout happens to have moved to: a task \
             cut from there would be verified against a commit nobody fetched"
        );
        let ours = fixture
            .checkouts()
            .into_iter()
            .find(|entry| entry.path == ready.worktree)
            .expect("git has this checkout registered, so another run can see it");
        assert_eq!(ours.head, drift.remote, "as the same commit");
        assert_eq!(
            ours.branch, None,
            "a task checkout is detached: a SHA, not somebody's branch"
        );
        assert_eq!(
            head_at(&fixture.project.root),
            drift.local,
            "the user's own checkout is untouched by any of it (VISION.md §10)"
        );
    }

    #[test]
    fn the_checkout_lives_outside_the_repository_it_was_cut_from() {
        let fixture = Fixture::new();
        let mut run = fixture.run();

        let ready = prepared(&mut run);

        assert!(
            !ready.worktree.starts_with(&fixture.project.root),
            "a task checkout inside the tree would be caught by the privacy scan and by \
             `is_clean` as untracked work: {} is not below {}",
            ready.worktree.display(),
            fixture.project.root.display()
        );
        assert!(
            ready.worktree.join("seed.txt").is_file(),
            "the checkout is a real checkout of the base commit, not an empty directory"
        );
    }

    #[test]
    fn a_prepared_task_holds_the_repository_lock_until_the_task_is_handed_back() {
        let fixture = Fixture::new();
        let mut run = fixture.run();

        let ready = prepared(&mut run);

        assert_eq!(
            ready.lock.path(),
            fixture.lock_file(),
            "what is held is this project's own repository lock, in its state directory"
        );
        assert!(fixture.lock_file().is_file(), "the lock file is there");
        assert!(
            ready.reclaimed().is_none(),
            "the lock was free, so this is a taking rather than a takeover: {ready:?}"
        );
        let refused = lock::acquire(&fixture.project.state_dir, Duration::ZERO)
            .expect_err("one repository is not run twice at once");
        assert!(
            refused
                .to_string()
                .contains(&std::process::id().to_string()),
            "the refusal waits behind this very process, which is the run that took it: \
             {refused}"
        );

        drop(ready);

        assert!(
            lock_is_free(&fixture.project),
            "the lock leaves with the task it was taken for"
        );
    }

    #[test]
    fn a_refused_preflight_ends_the_task_and_leaves_the_lock_free() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.work().join("loose.rs"), "uncommitted\n")
            .expect("a file is writable in the work tree");
        let mut run = fixture.run();
        let work = task();

        let refused = run
            .prepare(&work)
            .expect_err("a dirty checkout is not a world that has been proved sane");

        assert_eq!(
            kinds(&fixture.project, work.id),
            ["PreflightStarted", "PreflightFailed"],
            "the refusal is journaled as the end of this task, and it ends the run of \
             checks rather than appending a pass behind it"
        );
        let (class, evidence) = refusal(&fixture.project, work.id);
        assert_eq!(
            class,
            FailureClass::PolicyFailure,
            "the class the check named is the class the journal holds"
        );
        assert!(evidence.contains("loose.rs"), "{evidence}");
        assert!(
            refused.to_string().contains("mainline")
                && refused.to_string().contains("PolicyFailure"),
            "the caller is told which check refused and what it was classified as: \
             {refused}"
        );
        assert!(
            !fixture.lock_file().exists(),
            "a task that was refused leaves no lock file behind"
        );
        assert!(
            lock_is_free(&fixture.project),
            "and the lock is free for whoever the refusal sends the task to"
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "no checkout is cut for a task that was refused: {:?}",
            fixture.checkouts()
        );
    }

    #[test]
    fn a_task_asked_for_twice_gets_the_checkout_it_already_had() {
        let fixture = Fixture::new();
        let mut run = fixture.run();

        let first = prepared(&mut run);
        let evidence = first.worktree.join("attempt-work.txt");
        fs::write(&evidence, "the attempt that stopped\n").expect("a checkout is writable");
        let cut = first.worktree.clone();
        drop(first);

        let again = prepared(&mut run);

        assert_eq!(
            again.worktree, cut,
            "one task is one checkout: a remediation continues in the directory that \
             stopped (VISION.md §7) instead of orphaning it"
        );
        assert_eq!(
            fs::read_to_string(&evidence).expect("the work is still there"),
            "the attempt that stopped\n",
            "asking again changes nothing in a checkout that holds work"
        );
        assert_eq!(
            fixture.checkouts().len(),
            2,
            "the user's checkout and this task's, not one per attempt: {:?}",
            fixture.checkouts()
        );
    }
}

#[cfg(test)]
mod report {
    //! The round trip one attempt's report makes: the path the prompt names, the
    //! directory that exists before the provider is started, and the reading done
    //! after it exits.
    //!
    //! Named after the two methods it tests — [`Runner::prepare_report`] and
    //! [`Runner::read_report`] — the way `mod new` and `mod prepare` are named
    //! after the methods they test, and because the task that asked for this pair
    //! fixed `test(/report::/)` as its Verify command.

    use super::Runner;
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, EventKind, FailureClass, Project, ReportClaim, ReportResult, Task, TaskId,
        assemble, parse_plan, project_config_path,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The settings a run is buildable from: the mandatory gate, an adapter this
    /// build has, and a disk floor no test machine can breach.
    const BASE: &str = concat!(
        "provider = \"claude\"\n",
        "verify_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n",
        "min_free_disk_bytes = 1\n",
    );

    /// The queue position [`parse_plan`] gives the one-row plan below, and so the
    /// task every attempt of these fixtures is opened for. Asserted in [`task`],
    /// because a fixture that journalled a base for one task and opened an attempt
    /// on another would be testing the refusal rather than the round trip.
    const TASK: u32 = 1;

    /// The queue length the fixtures hand the header, so the prompt under test
    /// names a queue rather than a single task.
    const TOTAL: usize = 161;

    /// What a session that finished leaves in its report.
    const DONE: &str = "KTASK_RESULT: DONE\nSummary: written where the header said.\n";

    /// What a session that stopped short leaves in its report.
    const FAILED: &str = "KTASK_RESULT: FAILED\nSummary: the gate still refuses.\n";

    /// The context document and template the fixtures assemble with. Neither is
    /// read from anywhere: [`assemble`] is the half that reads nothing, so the
    /// path in the header is the only thing in these prompts that comes from
    /// outside the strings below.
    const CONTEXT: &str = "# Project context\n\nRead VISION.md first.";
    const TEMPLATE: &str = "# Task\n\n{{TASK}}\n\nWrite your report to the path the header names.";

    /// The queue row both attempts are opened for.
    fn task() -> Task {
        let document = "\
## T090 Report path and round-trip

**Outcome:** the agent's report is written where the prompt says and read back.
**Done-when:** a missing report is a classified failure naming the expected path.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/report::/)'`
**Refs:** VISION.md section 3 invariant 4
";
        let parsed = parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        let row = parsed
            .into_iter()
            .next()
            .expect("the fixture plan holds one row");
        assert_eq!(
            row.id,
            TaskId::new(TASK),
            "the fixtures journal a base and name paths for task {TASK}",
        );
        row
    }

    /// A registered project: a repository of its own, and a state directory of its
    /// own to hold a journal, a lock and an attempt's evidence.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
    }

    impl Fixture {
        /// A project holding `BASE` as its own settings document, with the state
        /// directory a registration would have made.
        fn new() -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::write(project_config_path(&project), BASE)
                .expect("a project settings document is writable");
            Self { repo, project }
        }

        /// The commit a preflight would have handed this run.
        fn base(&self) -> String {
            self.repo.seed_sha().to_owned()
        }

        /// The run this project is configured to have.
        fn run(&self) -> Runner {
            Runner::new(self.project.clone()).expect("a registered, configured project opens a run")
        }

        /// Journal what preflight journals, so an attempt has a base to open on.
        ///
        /// Written by the run's own recorder because that is who records a preflight
        /// verdict in the lifecycle; see `mod new` for the same fixture step.
        fn give_base(&self, run: &mut Runner) {
            run.recorder
                .record(
                    Some(TaskId::new(TASK)),
                    EventKind::PreflightPassed {
                        base_sha: self.base(),
                    },
                )
                .expect("a preflight verdict is journalable");
        }

        /// Where one attempt's report is spelled to live, written out of the state
        /// directory by hand rather than through the function under test — a
        /// fixture that called `report_path` would agree with the code whatever it
        /// did.
        fn report_of(&self, task: u32, attempt: u32) -> PathBuf {
            self.project
                .state_dir
                .join("attempts")
                .join(task.to_string())
                .join(attempt.to_string())
                .join("agent-report.md")
        }

        /// The prompt one attempt of this project's task is handed.
        fn prompt(&self, attempt: u32) -> String {
            assemble(
                &self.project,
                &task(),
                CONTEXT,
                &[],
                TEMPLATE,
                AttemptId::new(attempt),
                TOTAL,
            )
        }
    }

    /// What a session leaves at the path it was told to write to.
    ///
    /// It writes and nothing else: making the parent directory is the run's job, and
    /// doing it here would let a test pass over an attempt whose report directory was
    /// never created.
    fn agent_writes(path: &Path, words: &str) {
        fs::write(path, words).unwrap_or_else(|why| panic!("`{}`: {why}", path.display()));
    }

    /// The three answers of a [`ReportClaim::Claimed`], refused for anything else.
    fn claim(reading: &ReportClaim) -> (&Path, ReportResult, &str) {
        match reading {
            ReportClaim::Claimed { path, result, text } => (path, *result, text),
            other @ ReportClaim::Missing { .. } => {
                panic!("expected a claim about a report, got {other:?}")
            }
        }
    }

    /// The path, class and words of a [`ReportClaim::Missing`], refused for
    /// anything else.
    fn refusal(reading: &ReportClaim) -> (&Path, FailureClass, &str) {
        match reading {
            ReportClaim::Missing {
                path,
                class,
                detail,
            } => (path, *class, detail),
            other @ ReportClaim::Claimed { .. } => {
                panic!("expected a refusal to find a report, got {other:?}")
            }
        }
    }

    #[test]
    fn the_report_directory_is_made_before_the_provider_that_writes_it_starts() {
        let fixture = Fixture::new();
        let run = fixture.run();
        let attempt = AttemptId::new(7);

        let path = run
            .prepare_report(TaskId::new(TASK), attempt)
            .expect("preparing a report is a directory being made");
        let directory = path.parent().expect("a report sits in a directory");

        assert_eq!(
            path.strip_prefix(&fixture.project.state_dir).ok(),
            Some(Path::new("attempts/1/7/agent-report.md")),
            "the path the run hands the provider step is the attempt's own report below the \
             state directory, in the layout the evidence uses: {path:?}"
        );
        assert!(
            directory.is_dir(),
            "`{}` has to exist before a session is started, because the prompt tells that \
             session to write a file into it and the filesystem does not make a parent out \
             of an agent's good intentions",
            directory.display(),
        );
        assert_eq!(
            fs::metadata(directory)
                .unwrap_or_else(|why| panic!("`{}`: {why}", directory.display()))
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "an attempt's report is the least shareable thing it produces, so its directory \
             is owner-only whatever the machine's umask was: {}",
            directory.display(),
        );
    }

    #[test]
    fn the_prepared_path_is_the_path_the_prompt_names() {
        let fixture = Fixture::new();
        let run = fixture.run();

        let path = run
            .prepare_report(TaskId::new(TASK), AttemptId::new(2))
            .expect("a registered project has a report path");
        let prompt = fixture.prompt(2);

        assert!(
            prompt.contains(&format!("Report: `{}`", path.display())),
            "the prompt an agent is handed and the path the run will read back have to be \
             one path, or a report written exactly where it was told is a report nobody \
             reads: {prompt}"
        );
    }

    #[test]
    fn a_report_written_where_the_prompt_told_the_agent_is_read_back_as_its_claim() {
        let fixture = Fixture::new();
        let run = fixture.run();
        let task = TaskId::new(TASK);
        let attempt = AttemptId::new(1);

        let path = run
            .prepare_report(task, attempt)
            .expect("the directory the prompt names is made before the provider runs");
        assert!(
            fixture.prompt(1).contains(&format!("`{}`", path.display())),
            "the round trip starts at the path the agent was told: {}",
            path.display(),
        );
        agent_writes(&path, DONE);

        let reading = run
            .read_report(task, attempt)
            .expect("a report that is there is readable");

        let (read, result, text) = claim(&reading);
        assert_eq!(result, ReportResult::Done, "the header says what it says");
        assert_eq!(read, path, "the claim was read from the prepared path");
        assert_eq!(
            text, DONE,
            "the whole report comes back, because the body of a `NEEDS_INPUT` report is what \
             the pause is built out of"
        );
    }

    #[test]
    fn an_attempt_with_no_report_is_a_failure_naming_the_path_the_prompt_named() {
        let fixture = Fixture::new();
        let run = fixture.run();
        let task = TaskId::new(TASK);

        let path = run
            .prepare_report(task, AttemptId::new(1))
            .expect("the directory is made even for an attempt that writes nothing");

        let reading = run
            .read_report(task, AttemptId::new(1))
            .expect("an absent report is an answer, not a failed read");

        let (expected, class, detail) = refusal(&reading);
        assert_eq!(
            expected, path,
            "the refusal has to name the very path the prompt named, which is the only \
             starting point a remediation has"
        );
        assert_eq!(
            expected,
            fixture.report_of(TASK, 1).as_path(),
            "and it is the attempt's own report file, not somewhere else the run chose"
        );
        assert_eq!(
            class,
            FailureClass::AgentFailure,
            "a session that ran and left no account of itself did not complete the work \
             (VISION.md §7); it is not a policy breach, a refused gate, or a question"
        );
        assert!(
            detail.contains(&expected.display().to_string()),
            "the words of the refusal carry the path too, because a class alone is not \
             something an operator can act on: {detail}"
        );
    }

    #[test]
    fn a_retry_is_read_as_its_own_report_and_never_the_one_its_predecessor_left() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        fixture.give_base(&mut run);
        let task = task();

        let first = run
            .begin_attempt(&task)
            .expect("an attempt opens on the base the journal named");
        let second = run
            .begin_attempt(&task)
            .expect("a retry opens after the attempt it remediates");
        assert_eq!(
            (first, second),
            (AttemptId::new(1), AttemptId::new(2)),
            "two attempts of one task are two attempts, numbered in the order they were opened"
        );

        let first_report = run
            .prepare_report(TaskId::new(TASK), first)
            .expect("the first attempt's report directory is made");
        let second_report = run
            .prepare_report(TaskId::new(TASK), second)
            .expect("the retry's report directory is made");
        assert_ne!(
            first_report, second_report,
            "one attempt never writes into the other's directory"
        );
        agent_writes(&first_report, DONE);

        let retry = run
            .read_report(TaskId::new(TASK), second)
            .expect("an attempt that wrote nothing is answered, not assumed");
        let (expected, class, _) = refusal(&retry);
        assert_eq!(
            expected, second_report,
            "the retry's own path is the one named — reading the previous attempt's report \
             here would report a DONE the retry never claimed"
        );
        assert_eq!(class, FailureClass::AgentFailure);

        agent_writes(&second_report, FAILED);
        let retry = run
            .read_report(TaskId::new(TASK), second)
            .expect("the retry's own report is there now");
        assert_eq!(
            claim(&retry).1,
            ReportResult::Failed,
            "the retry is read as what the retry said"
        );
        let earlier = run
            .read_report(TaskId::new(TASK), first)
            .expect("the first attempt's report is still on disk");
        assert_eq!(
            claim(&earlier).1,
            ReportResult::Done,
            "and a later attempt neither replaces nor erases the account of the one before it"
        );
    }

    #[test]
    fn preparing_a_report_directory_leaves_what_the_attempt_already_filed_alone() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        fixture.give_base(&mut run);
        let task = task();
        let attempt = run
            .begin_attempt(&task)
            .expect("an attempt files its evidence as it opens");

        let filed = fixture
            .report_of(TASK, attempt.get())
            .parent()
            .expect("a report sits in a directory")
            .join("report.md");
        let before =
            fs::read_to_string(&filed).unwrap_or_else(|why| panic!("`{}`: {why}", filed.display()));

        let path = run
            .prepare_report(TaskId::new(TASK), attempt)
            .expect("preparing the agent's report directory is not a re-write of the attempt");

        assert_eq!(
            fs::read_to_string(&filed).unwrap_or_else(|why| panic!("`{}`: {why}", filed.display())),
            before,
            "an attempt's generated record and the agent's account of itself are two files \
             in one directory, and making room for the second cannot touch the first"
        );
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("agent-report.md"),
            "and the two keep different names: {path:?}"
        );
    }
}

#[cfg(test)]
mod run_phase {
    //! One phase of a protocol, worked: the row that marks it, the prompt the
    //! session is handed, what the session was allowed to touch, and the account
    //! it left of itself.
    //!
    //! Named after the method it tests, the way `mod new`, `mod prepare` and
    //! `mod report` are named after theirs, because the task that asked for this
    //! step fixed `test(/runner::run_phase/)` as its Verify command.
    //!
    //! Every session here is the `dummy` adapter replaying a scenario file, which
    //! is what VISION.md §15 makes the deterministic way to drive the runner. The
    //! write the lifecycle rule turns on is a file a reviewer can read in the
    //! script — `forbidden.md` in a phase scoped to tests — rather than a claim
    //! this file makes about a session that never ran.

    use super::{PhaseOutcome, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Error, EventKind, FailureClass, Journal, Phase, PhaseSpec, Prepared, Project,
        ReportResult, Task, TaskId, WriteScope, parse_plan, project_config_path,
    };
    use std::fmt::Write;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The queue position [`parse_plan`] gives the one-row plan below, and so the
    /// task every phase here is worked for. Asserted in [`task`], because a
    /// fixture that scripted a session for one task and worked a phase for another
    /// would be testing the refusal rather than the phase.
    const TASK: u32 = 1;

    /// What a session that finished leaves in the report the prompt named.
    const DONE: &str = "KTASK_RESULT: DONE\nSummary: the phase is worked.\n";

    /// The words a session prints, as one scenario declares them: two lines, so a
    /// row per line is observable rather than assumed.
    const LINES: &str = "reading the code\nwriting the tests\n";

    /// The settings a phase runs under: the scripted adapter, its own scenario
    /// file, the mandatory gate, and a disk floor no test machine breaches.
    fn settings(scenario: &Path, extra: &str) -> String {
        format!(
            "provider = \"dummy\"\n\
             dummy_scenario_path = \"{}\"\n\
             verify_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n\
             min_free_disk_bytes = 1\n\
             {extra}",
            scenario.display()
        )
    }

    /// A scenario document's string, with the breaks an agent's text is full of
    /// spelled the way TOML wants them: the file a reviewer reads has to be the file
    /// the adapter loads, so a fixture cannot leave the escaping to chance.
    fn toml_text(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    /// The scenario document for one session that succeeds, prints `stdout`, and
    /// leaves `files` in the directory it runs in.
    fn scenario(files: &[(&str, &str)], stdout: &str) -> String {
        let mut document = format!(
            "[[steps]]\non_task = {TASK}\noutcome = \"success\"\nstdout = \"{printed}\"\n",
            printed = toml_text(stdout)
        );
        if !files.is_empty() {
            document.push_str("\n[steps.files]\n");
            for (path, contents) in files {
                writeln!(document, "\"{path}\" = \"{}\"", toml_text(contents))
                    .expect("a String always has room for what is written into it");
            }
        }
        document
    }

    /// A registered project, the scenario file its adapter reads, and the config
    /// home its prompt library is read from.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
        scenario: PathBuf,
        config_home: PathBuf,
    }

    impl Fixture {
        /// A project pointed at the scenario file [`Fixture::script`] will write,
        /// and at a configuration home of its own.
        fn new() -> Self {
            Self::with_settings("")
        }

        /// As [`Fixture::new`], with `extra` appended to the settings document —
        /// how one test configures a model id and the rest do not.
        fn with_settings(extra: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            let scenario = repo.path().join("scenario.toml");
            let config_home = repo.path().join("config-home");
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::write(project_config_path(&project), settings(&scenario, extra))
                .expect("a project settings document is writable");
            Self {
                repo,
                project,
                scenario,
                config_home,
            }
        }

        /// The script the adapter replays, written before the run is opened:
        /// [`crate::provider::build`] reads the file, so a scenario written after
        /// the run would be a scenario the run never saw.
        fn script(&self, document: &str) {
            fs::write(&self.scenario, document).expect("a scenario document is writable");
        }

        /// The run this project is configured to have.
        fn run(&self) -> Runner {
            Runner::new(self.project.clone()).expect("a registered, configured project opens a run")
        }

        /// The environment a phase's prompt is read from: a configuration home of
        /// the fixture's own, so no test aims the prompt library at the machine
        /// running it. `docs/DESIGN.md` Conventions keeps a test out of setting
        /// variables, and out of the operator's real configuration, by handing the
        /// accessor over instead.
        fn env(&self) -> impl Fn(&str) -> Option<String> {
            let home = self.config_home.clone();
            move |key: &str| (key == "XDG_CONFIG_HOME").then(|| home.display().to_string())
        }

        /// Where one attempt's report is spelled to live, written out of the state
        /// directory by hand rather than through the function under test — the way
        /// `mod report` spells it, for the same reason.
        fn report_of(&self, attempt: u32) -> PathBuf {
            self.project
                .state_dir
                .join("attempts")
                .join(TASK.to_string())
                .join(attempt.to_string())
                .join("agent-report.md")
        }

        /// Leave `words` at the path the prompt names, as a session that reported
        /// would have.
        fn report(&self, attempt: u32, words: &str) {
            let path = self.report_of(attempt);
            fs::create_dir_all(
                path.parent()
                    .expect("a report is spelled below an attempt directory"),
            )
            .expect("an attempt's report directory is creatable");
            fs::write(&path, words).expect("a report is writable");
        }

        /// The repository's own checkout, which a session has no business writing.
        fn checkout(&self) -> PathBuf {
            self.repo.work().to_path_buf()
        }
    }

    /// The queue's one task.
    fn task() -> Task {
        let document = "\
## T091 Runner step: run one protocol phase

**Outcome:** a phase runs the agent and checks what it was allowed to touch.
**Done-when:** a write outside the scope is a policy failure naming the paths.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::run_phase/)'`
**Refs:** VISION.md section 9
";
        let parsed = parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        let row = parsed
            .into_iter()
            .next()
            .expect("the fixture plan holds one row");
        assert_eq!(row.id, TaskId::new(TASK), "every fixture works task {TASK}");
        row
    }

    /// The phase under test, hand-spelled rather than read out of
    /// [`crate::protocol::direct`]: what a phase is allowed to write is the
    /// argument whose refusal these tests are about, and a fixture that took it
    /// from the declaration under test would agree with it whatever it said.
    fn phase(write_scope: WriteScope) -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Implement,
            write_scope,
            gate: None,
            records_evidence: false,
        }
    }

    /// Take a task as far as an attempt can start, so a phase has a checkout and a
    /// base to be measured against.
    fn prepared(run: &mut Runner) -> Prepared {
        run.prepare(&task())
            .expect("nothing in this fixture gives preflight a reason to refuse")
    }

    /// Work one phase of the queue's task, with the prompt read from the fixture's
    /// own configuration home.
    fn work(
        fixture: &Fixture,
        run: &mut Runner,
        attempt: u32,
        write_scope: WriteScope,
    ) -> crate::Result<PhaseOutcome> {
        let ready = prepared(run);
        run.run_phase_with(
            &fixture.env(),
            &ready,
            &task(),
            AttemptId::new(attempt),
            &phase(write_scope),
        )
    }

    /// The kinds the journal holds for the queue's task, oldest first, read on a
    /// second connection — the way the TUI and a recovery walk read them.
    fn kinds(project: &Project) -> Vec<&'static str> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// The one `AttemptFinished` row the journal holds, refused when there is not
    /// exactly one.
    fn finished(
        project: &Project,
    ) -> (
        AttemptId,
        i32,
        Option<crate::Usage>,
        Option<String>,
        Option<String>,
    ) {
        let rows = Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable");
        let mut found: Vec<_> = Vec::new();
        for row in rows {
            if let EventKind::AttemptFinished {
                attempt,
                exit_code,
                usage,
                session_id,
                model_reported,
            } = row.kind
            {
                found.push((attempt, exit_code, usage, session_id, model_reported));
            }
        }
        assert_eq!(
            found.len(),
            1,
            "one session ends in exactly one row saying so, not one per line it printed"
        );
        found.remove(0)
    }

    /// The four answers of a [`PhaseOutcome::Claimed`], refused for anything else.
    fn claimed(outcome: &PhaseOutcome) -> (Phase, AttemptId, ReportResult, &str, &[PathBuf]) {
        match outcome {
            PhaseOutcome::Claimed {
                phase,
                attempt,
                report,
                text,
                changed,
            } => (*phase, *attempt, *report, text, changed),
            other @ PhaseOutcome::Unreported { .. } => {
                panic!("expected a report read back as a claim, got {other:?}")
            }
        }
    }

    /// The four answers of a [`PhaseOutcome::Unreported`], refused for anything
    /// else.
    fn unreported(outcome: &PhaseOutcome) -> (Phase, AttemptId, FailureClass, &Path, &str) {
        match outcome {
            PhaseOutcome::Unreported {
                phase,
                attempt,
                class,
                path,
                detail,
            } => (*phase, *attempt, *class, path, detail),
            other @ PhaseOutcome::Claimed { .. } => {
                panic!("expected a phase that left no report, got {other:?}")
            }
        }
    }

    /// The `Error::Policy`'s words and paths, refused for anything else.
    fn policy(outcome: &crate::Result<PhaseOutcome>) -> (String, Vec<PathBuf>) {
        match outcome {
            Err(Error::Policy { detail, paths }) => (detail.clone(), paths.clone()),
            Err(other) => panic!("expected a policy failure, got {other}"),
            Ok(claimed) => {
                panic!("expected a policy failure, got a phase that returned {claimed:?}")
            }
        }
    }

    #[test]
    fn a_phase_that_writes_outside_its_scope_is_refused_naming_the_path() {
        let fixture = Fixture::new();
        fixture.script(&scenario(
            &[("forbidden.md", "touched by the session\n")],
            LINES,
        ));
        fixture.report(1, DONE);
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 1, WriteScope::TestsOnly);

        let (detail, paths) = policy(&outcome);
        assert!(
            paths.contains(&PathBuf::from("forbidden.md")),
            "the refusal has to name what it refused, and it named {paths:?}"
        );
        assert!(
            detail.contains("write scope"),
            "the words say which rule was broken, so a reader is not left to guess: {detail}"
        );
        assert!(
            kinds(&fixture.project).ends_with(&[
                "PhaseEntered",
                "AgentOutput",
                "AgentOutput",
                "AttemptFinished"
            ]),
            "the session really ran and ended — the refusal is about what it touched, \
             not about a call that never happened: {:?}",
            kinds(&fixture.project)
        );
    }

    #[test]
    fn a_phase_that_writes_only_where_its_scope_allows_is_not_refused() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[("tests/new_test.rs", "a test\n")], LINES));
        fixture.report(1, DONE);
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 1, WriteScope::TestsOnly);

        let (phase, attempt, report, text, changed) = claimed(
            outcome
                .as_ref()
                .expect("a write the scope grants is not a refusal"),
        );
        assert_eq!(phase, Phase::Implement);
        assert_eq!(attempt, AttemptId::new(1));
        assert_eq!(report, ReportResult::Done);
        assert_eq!(text, DONE);
        assert!(
            changed.contains(&PathBuf::from("tests/new_test.rs")),
            "the outcome says what the phase changed, and it said {changed:?}"
        );
    }

    #[test]
    fn a_phase_that_left_no_report_is_a_classified_failure_and_never_a_claim() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[], LINES));
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 1, WriteScope::All);

        let (phase, attempt, class, path, detail) = unreported(
            outcome
                .as_ref()
                .expect("a session that wrote no report is an outcome, not an error"),
        );
        assert_eq!(phase, Phase::Implement);
        assert_eq!(attempt, AttemptId::new(1));
        assert_eq!(
            class,
            FailureClass::AgentFailure,
            "the class the recovery policy is read from is reported, not re-derived"
        );
        assert_eq!(
            path,
            fixture.report_of(1),
            "the refusal names the path the prompt told the agent to write"
        );
        assert!(
            detail.contains("agent-report.md"),
            "and says so in its own words: {detail}"
        );
        assert_eq!(
            kinds(&fixture.project),
            [
                "PreflightStarted",
                "PreflightPassed",
                "PhaseEntered",
                "AgentOutput",
                "AgentOutput",
                "AttemptFinished",
            ],
            "a silent session leaves the session's rows and no claim of completion"
        );
    }

    #[test]
    fn a_phase_hands_the_journal_its_entry_then_its_lines_then_its_session_end() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[], LINES));
        fixture.report(1, DONE);
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 1, WriteScope::All);

        let (phase, attempt, report, _, changed) = claimed(
            outcome
                .as_ref()
                .expect("a reported phase hands back what the agent wrote"),
        );
        assert_eq!(
            (phase, attempt, report),
            (Phase::Implement, AttemptId::new(1), ReportResult::Done)
        );
        assert!(
            changed.is_empty(),
            "the session touched nothing: {changed:?}"
        );
        assert_eq!(
            kinds(&fixture.project),
            [
                "PreflightStarted",
                "PreflightPassed",
                "PhaseEntered",
                "AgentOutput",
                "AgentOutput",
                "AttemptFinished",
            ],
            "the phase is marked before the session, its lines stand one per row, and \
             its end closes the row set"
        );

        let rows = Journal::open_for(&fixture.project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable");
        let lines: Vec<(u32, String)> = rows
            .iter()
            .filter_map(|row| match &row.kind {
                EventKind::AgentOutput { attempt, text, .. } => Some((attempt.get(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                (1, "reading the code".to_owned()),
                (1, "writing the tests".to_owned()),
            ],
            "one row per line the session printed, each attributed to the attempt that \
             printed it, and no row for the empty text after the last break"
        );
        for row in &rows {
            assert_eq!(
                row.task_id,
                Some(TaskId::new(TASK)),
                "every row of a phase is \
             the task's own: {:?}",
                row.kind
            );
        }
    }

    #[test]
    fn a_session_that_reported_nothing_leaves_nothing_in_the_row_that_records_its_end() {
        let fixture = Fixture::with_settings("model = \"gpt-5.6-sol\"\n");
        fixture.script(&scenario(&[], "worked\n"));
        fixture.report(1, DONE);
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 1, WriteScope::All);

        assert!(
            outcome.is_ok(),
            "a session that reported no model contradicts nothing: {:?}",
            outcome.as_ref().err()
        );
        let (attempt, exit_code, usage, session_id, model_reported) = finished(&fixture.project);
        assert_eq!(attempt, AttemptId::new(1));
        assert_eq!(exit_code, 0, "the status the session's own process left");
        assert!(usage.is_none(), "nothing measured this session");
        assert!(session_id.is_none(), "the session disclosed no identifier");
        assert_eq!(
            model_reported, None,
            "the configured id is never copied into the field that means \
             'what the session said it ran on'"
        );
    }

    #[test]
    fn a_scope_violation_and_a_missing_report_together_are_answered_as_the_violation() {
        let fixture = Fixture::new();
        fixture.script(&scenario(
            &[("forbidden.md", "touched by the session\n")],
            LINES,
        ));
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 1, WriteScope::TestsOnly);

        let (detail, paths) = policy(&outcome);
        assert!(
            paths.contains(&PathBuf::from("forbidden.md")),
            "a phase that both wrote outside its scope and stayed silent is refused for \
             the write: {paths:?}"
        );
        assert!(detail.contains("write scope"), "{detail}");
    }

    #[test]
    fn a_phase_whose_prompt_cannot_be_read_starts_no_session() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[("written.md", "by the session\n")], "worked\n"));
        fs::create_dir_all(fixture.config_home.join("ktask-rs"))
            .expect("the configuration home is creatable");
        fs::write(
            fixture.config_home.join("ktask-rs/prompts"),
            "not a directory\n",
        )
        .expect("the library path is occupiable by a file");
        let mut run = fixture.run();
        let ready = prepared(&mut run);

        let outcome = run.run_phase_with(
            &fixture.env(),
            &ready,
            &task(),
            AttemptId::new(1),
            &phase(WriteScope::All),
        );

        assert!(
            matches!(outcome, Err(Error::Policy { .. })),
            "a prompt library that is not a private directory is a refusal, got \
             {outcome:?}"
        );
        assert!(
            !ready.worktree.join("written.md").exists(),
            "a phase with no prompt starts no session, so the scenario's file is \
             nowhere to be found"
        );
        assert_eq!(
            kinds(&fixture.project),
            ["PreflightStarted", "PreflightPassed", "PhaseEntered"],
            "the phase is marked, and nothing after it was written"
        );
    }

    #[test]
    fn a_phase_that_cannot_read_its_decisions_starts_no_session() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[("written.md", "by the session\n")], "worked\n"));
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fs::create_dir_all(fixture.checkout().join("docs"))
            .expect("the repository's docs directory is creatable");
        fs::write(fixture.checkout().join("docs/adr"), "not a directory\n")
            .expect("the decisions path is occupiable by a file");

        let outcome = run.run_phase_with(
            &fixture.env(),
            &ready,
            &task(),
            AttemptId::new(1),
            &phase(WriteScope::All),
        );

        assert!(
            matches!(outcome, Err(Error::Policy { .. })),
            "a decision archive in the wrong shape refuses the prompt rather than \
             handing out one with decisions missing: {outcome:?}"
        );
        assert!(
            !ready.worktree.join("written.md").exists(),
            "and it refuses before a token is spent"
        );
    }

    #[test]
    fn a_phase_runs_its_session_in_the_tasks_checkout_and_not_the_repository() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[("written.md", "by the session\n")], "worked\n"));
        fixture.report(1, DONE);
        let mut run = fixture.run();

        let ready = prepared(&mut run);
        let outcome = run.run_phase_with(
            &fixture.env(),
            &ready,
            &task(),
            AttemptId::new(1),
            &phase(WriteScope::All),
        );

        let (_, _, _, _, changed) = claimed(
            outcome
                .as_ref()
                .expect("an implement phase may write anywhere in its own checkout"),
        );
        assert!(
            changed.contains(&PathBuf::from("written.md")),
            "the write is what the phase's scope is measured against: {changed:?}"
        );
        assert!(
            ready.worktree.join("written.md").is_file(),
            "the session ran in the task's checkout, where the attempt is worked: {}",
            ready.worktree.display()
        );
        assert!(
            !fixture.checkout().join("written.md").exists(),
            "and never in the repository the run supervises, whose checkout the \
             scenario's file would have dirtied"
        );
    }

    #[test]
    fn a_phase_is_worked_for_the_attempt_it_was_given() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[], LINES));
        fixture.report(2, DONE);
        let mut run = fixture.run();

        let outcome = work(&fixture, &mut run, 2, WriteScope::All);

        let (_, attempt, report, _, _) = claimed(
            outcome
                .as_ref()
                .expect("a retry is worked as the attempt it was given"),
        );
        assert_eq!(attempt, AttemptId::new(2));
        assert_eq!(report, ReportResult::Done);
        assert_eq!(
            finished(&fixture.project).0,
            AttemptId::new(2),
            "the row that ends a session names the attempt that ended"
        );
        assert!(
            !fixture.report_of(1).exists(),
            "the report read back is the one attempt 2 was told to write, and attempt 1 \
             left none for this phase to mistake for it"
        );
    }
}

#[cfg(test)]
mod gate_phase {
    //! One phase's mechanical check, run here rather than trusted from a report:
    //! the gate pair journalled around the command, the red and green verdicts read
    //! out of two test summaries, and the evidence a phase leaves beside the attempt
    //! it was run for.
    //!
    //! Named after the method it tests, the way `mod new`, `mod prepare`,
    //! `mod report` and `mod run_phase` are named after theirs, because the task
    //! that asked for this step fixed `test(/runner::gate_phase/)` as its Verify
    //! command.
    //!
    //! The gate every test here runs is a `/bin/sh` script that prints a
    //! cargo-shaped report — the text of one, in a file outside every checkout — and
    //! exits the way that report reads. Running a real `cargo test` would make each
    //! of these tests depend on a workspace compiling at the instant it ran, while
    //! what the runner decides on is the *report* and the exit status. Reading a
    //! report correctly is [`crate::parse_cargo`]'s own coverage.

    use super::{Prepared, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Config, Error, EventKind, GateKind, GateResult, Journal, Phase, PhaseSpec,
        Project, Task, TaskId, TddException, TestSummary, WriteScope, evidence_dir, for_task,
        parse_plan, project_config_path,
    };
    use serde_json::Value;
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The queue position [`parse_plan`] gives the one-row plan below, and so the
    /// task every phase here is gated for.
    const TASK: u32 = 1;

    /// The attempt every phase here is filed under. The fixtures file under a
    /// hand-numbered one rather than calling [`Runner::begin_attempt`] so that the
    /// rows a test counts are the rows this step wrote.
    const ATTEMPT: u32 = 1;

    /// The rows a prepared task has already left when its phase is gated: the
    /// preflight, and nothing else.
    const PREPARED: [&str; 2] = ["PreflightStarted", "PreflightPassed"];

    /// The rows a phase that ran its gate leaves: the pair, beside the preflight.
    const GATED: [&str; 4] = [
        "PreflightStarted",
        "PreflightPassed",
        "GateStarted",
        "GateFinished",
    ];

    /// Which command a fixture's settings document names as `targeted_test_command`.
    #[derive(Clone, Copy)]
    enum Command {
        /// The script [`Fixture`] writes, which prints a report and exits as it
        /// reads.
        Script,
        /// Nothing: a project that configured no targeted gate at all.
        Unset,
        /// A program that is not there, so the gate cannot be started.
        Nowhere,
    }

    /// The settings a gate runs under: an adapter this build has, the mandatory
    /// verify gate, whatever `gate` names as the targeted command, and a disk floor
    /// no test machine breaches.
    fn settings(gate: &str, extra: &str) -> String {
        format!(
            "provider = \"claude\"\n\
             verify_command = [\"/bin/sh\", \"-c\", \"exit 0\"]\n\
             {gate}\
             min_free_disk_bytes = 1\n\
             {extra}"
        )
    }

    /// The `targeted_test_command` line a settings document runs `words` with.
    fn gate_line(words: &[&str]) -> String {
        let quoted = words
            .iter()
            .map(|word| format!("\"{word}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!("targeted_test_command = [{quoted}]\n")
    }

    /// The gate script: print the report, or say on standard error that there is
    /// none to print; hand the report to standard error and refuse when a marker
    /// file says this command writes its results there; otherwise exit the way the
    /// report reads, because a cargo test run exits non-zero on a failing test.
    fn gate_script(report: &Path, stderr_marker: &Path, exit_marker: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             REPORT='{}'\n\
             if [ ! -f \"$REPORT\" ]; then echo 'gate: nothing to print' >&2; exit 2; fi\n\
             if [ -f '{}' ]; then cat \"$REPORT\" >&2; exit 1; fi\n\
             cat \"$REPORT\"\n\
             if [ -f '{}' ]; then read code < '{}'; exit \"$code\"; fi\n\
             if grep -q '^test result: FAILED' \"$REPORT\"; then exit 1; fi\n\
             exit 0\n",
            report.display(),
            stderr_marker.display(),
            exit_marker.display(),
            exit_marker.display()
        )
    }

    /// A registered project, the script its targeted gate runs, and the file that
    /// script prints as its test report.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
        script: PathBuf,
        report: PathBuf,
        stderr_marker: PathBuf,
        exit_marker: PathBuf,
    }

    impl Fixture {
        /// A project whose targeted gate is the script this fixture writes.
        fn new() -> Self {
            Self::with_gate(Command::Script, "")
        }

        /// As [`Fixture::new`], with `extra` appended to the settings document —
        /// how one test sets `secret_patterns` and the rest do not.
        fn with_settings(extra: &str) -> Self {
            Self::with_gate(Command::Script, extra)
        }

        /// A project that configured no `targeted_test_command`, so a phase's
        /// declaration names a gate the profile holds no command for.
        fn without_gate_command() -> Self {
            Self::with_gate(Command::Unset, "")
        }

        /// A project whose targeted gate names a program that is not there.
        fn with_unspawnable_gate() -> Self {
            Self::with_gate(Command::Nowhere, "")
        }

        /// A project with `targeted_test_command` decided by `command`, and `extra`
        /// beside the base settings.
        fn with_gate(command: Command, extra: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            let script = repo.path().join("gate.sh");
            let report = repo.path().join("report.txt");
            let stderr_marker = repo.path().join("report-on-stderr");
            let exit_marker = repo.path().join("exit-code");
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::write(&script, gate_script(&report, &stderr_marker, &exit_marker))
                .expect("a gate script is writable");
            let gate = match command {
                Command::Script => gate_line(&["/bin/sh", &script.display().to_string()]),
                Command::Unset => String::new(),
                Command::Nowhere => gate_line(&["/nonexistent/ktask-gate-program"]),
            };
            fs::write(project_config_path(&project), settings(&gate, extra))
                .expect("a project settings document is writable");
            Self {
                repo,
                project,
                script,
                report,
                stderr_marker,
                exit_marker,
            }
        }

        /// The run this project is configured to have.
        fn run(&self) -> Runner {
            Runner::new(self.project.clone()).expect("a registered, configured project opens a run")
        }

        /// What the gate's command will print as its test report.
        fn report(&self, text: &str) {
            fs::write(&self.report, text).expect("a test report is writable");
        }

        /// Make the command carry its report on standard error and refuse, as a
        /// command that explains itself there does.
        fn report_on_stderr(&self) {
            fs::write(&self.stderr_marker, "").expect("a marker file is writable");
        }

        /// Make the command exit `code` whatever its report says.
        fn exits_with(&self, code: u8) {
            fs::write(&self.exit_marker, format!("{code}\n")).expect("an exit marker is writable");
        }

        /// Where one attempt's evidence for `phase` is spelled to live.
        fn evidence_of(&self, phase: &str) -> PathBuf {
            evidence_dir(&self.project, TaskId::new(TASK), AttemptId::new(ATTEMPT))
                .join("phases")
                .join(format!("{phase}.jsonl"))
        }

        /// Every line that file holds, oldest first.
        fn filed(&self, phase: &str) -> Vec<Value> {
            let path = self.evidence_of(phase);
            let text = fs::read_to_string(&path).unwrap_or_else(|why| {
                panic!(
                    "`{}` should hold what the phase filed: {why}",
                    path.display()
                )
            });
            text.lines()
                .map(|line| serde_json::from_str(line).expect("a filed line is JSON"))
                .collect()
        }
    }

    /// The queue's one task.
    fn task() -> Task {
        let document = "\
## T092 Runner step: TDD phase gating

**Outcome:** the red and green phases are enforced by the runner, not trusted.
**Done-when:** a red phase that fails no new test does not advance.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::gate_phase/)'`
**Refs:** VISION.md section 9
";
        let parsed = parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        let row = parsed
            .into_iter()
            .next()
            .expect("the fixture plan holds one row");
        assert_eq!(row.id, TaskId::new(TASK), "every fixture works task {TASK}");
        row
    }

    /// The same row, working `tdd` and declaring §9's exception to test-first.
    ///
    /// The declaration is appended to the body rather than stored in a field, the
    /// way `protocol.rs`'s own fixtures spell it: the section is a fact about a
    /// queue row, and a run reads it back out of the row's text.
    fn excused() -> Task {
        let mut row = task();
        row.protocol = Some("tdd".to_owned());
        row.body
            .push_str("**Tdd-exception:** Documentation\nOnly a doc comment moved.\n");
        row
    }

    /// A phase that declares the targeted gate, hand-spelled the way `mod
    /// run_phase` spells its phase: which gate decides a phase, what it may write
    /// and whether it records evidence are the arguments these tests turn on, and
    /// a fixture that took them from the declaration under test would agree with
    /// whatever that declaration said.
    fn declared(step: Phase, scope: WriteScope, records: bool) -> PhaseSpec {
        PhaseSpec {
            phase: step,
            write_scope: scope,
            gate: Some(GateKind::Targeted),
            records_evidence: records,
        }
    }

    /// §9's red phase: tests only, targeted gate, evidence recorded.
    fn red() -> PhaseSpec {
        declared(Phase::Red, WriteScope::TestsOnly, true)
    }

    /// §9's green phase: the whole tree, the same gate, evidence recorded.
    fn green() -> PhaseSpec {
        declared(Phase::Green, WriteScope::All, true)
    }

    /// §9's refactor phase: the same gate, no evidence of its own.
    fn refactor() -> PhaseSpec {
        declared(Phase::Refactor, WriteScope::All, false)
    }

    /// A green phase that declares no gate, so nothing decides it.
    fn ungated() -> PhaseSpec {
        PhaseSpec {
            phase: Phase::Green,
            write_scope: WriteScope::All,
            gate: None,
            records_evidence: true,
        }
    }

    /// A cargo-shaped report: one test binary that opened, named its failures under
    /// the `failures:` block, and answered with the result line at column zero.
    fn report(passed: u32, failing: &[&str]) -> String {
        let failed = u32::try_from(failing.len()).unwrap_or(u32::MAX);
        let mut text = format!("running {} tests\n", passed + failed);
        for name in failing {
            writeln!(&mut text, "test {name} ... FAILED")
                .expect("a String is a writer that never refuses");
        }
        if !failing.is_empty() {
            text.push_str("failures:\n");
            for name in failing {
                writeln!(&mut text, "    {name}").expect("a String is a writer that never refuses");
            }
        }
        let verdict = if failing.is_empty() { "ok" } else { "FAILED" };
        write!(
            &mut text,
            "\ntest result: {verdict}. {passed} passed; {failed} failed; 0 ignored; 0 measured; \
             0 filtered out; finished in 0.00s\n"
        )
        .expect("a String is a writer that never refuses");
        text
    }

    /// The summary a phase starts from, as [`report`]'s run would have been parsed.
    fn summary(passed: u32, failing: &[&str]) -> TestSummary {
        TestSummary {
            passed,
            failed: u32::try_from(failing.len()).unwrap_or(u32::MAX),
            ignored: 0,
            failures: failing.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    /// Take a task as far as an attempt can start, so a phase has a checkout and a
    /// base to be measured against.
    fn prepared(run: &mut Runner) -> Prepared {
        run.prepare(&task())
            .expect("nothing in this fixture gives preflight a reason to refuse")
    }

    /// Gate one phase of the queue's task, at the attempt every fixture files under.
    fn gate(
        run: &mut Runner,
        ready: &Prepared,
        spec: &PhaseSpec,
        before: Option<&TestSummary>,
    ) -> crate::Result<TestSummary> {
        run.gate_phase(ready, &task(), AttemptId::new(ATTEMPT), spec, before)
    }

    /// As [`gate`], for `work` as the queue row describes it — how a test declares the
    /// `**Tdd-exception:**` a phase is expected to honour.
    fn gate_for(
        run: &mut Runner,
        ready: &Prepared,
        work: &Task,
        spec: &PhaseSpec,
        before: Option<&TestSummary>,
    ) -> crate::Result<TestSummary> {
        run.gate_phase(ready, work, AttemptId::new(ATTEMPT), spec, before)
    }

    /// The kinds the journal holds for the task, oldest first, read on a second
    /// connection because that is who asks this question in real life.
    fn kinds(project: &Project) -> Vec<&'static str> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// What the phase's `GateFinished` row says the command did.
    fn finished(project: &Project) -> GateResult {
        for row in Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
        {
            if let EventKind::GateFinished { result } = row.kind {
                return result;
            }
        }
        panic!("a gate that ran leaves the row that records what it produced");
    }

    /// The exception row an excused phase left, as its category and its reason.
    fn claimed(project: &Project) -> (TddException, String) {
        for row in Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
        {
            if let EventKind::TddExceptionUsed { exception, reason } = row.kind {
                return (exception, reason);
            }
        }
        panic!("an excused phase leaves the row that records the exception and its reason");
    }

    /// Leave `text` at `path` inside the task's checkout, as a session that wrote
    /// there would have.
    fn write_in(ready: &Prepared, path: &str, text: &str) {
        let full = ready.worktree.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("a checkout directory is creatable");
        }
        fs::write(&full, text).expect("a checkout file is writable");
    }

    /// A checkout file whose mode was taken away, put back when dropped.
    struct Unreadable {
        path: PathBuf,
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o644));
        }
    }

    /// Make `path` unreadable even to its owner, until the returned guard is dropped.
    ///
    /// The filesystem's own `PermissionDenied` is the one refusal a fixture cannot be
    /// handed: a phase that cannot read a changed path has to say so rather than hash
    /// it as absent, and nothing shorter than taking the mode away asks that question.
    /// The same trick `attempt.rs` plays on a state directory it cannot look inside.
    fn unreadable(path: &Path) -> Unreadable {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap_or_else(|why| {
            panic!("`{}` should take mode 000: {why}", path.display());
        });
        Unreadable {
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn a_red_phase_that_newly_fails_is_told_the_failure_it_asked_for() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        let after = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect("a test that failed and was not failing before is the failure red asked for");
        assert_eq!(after.failures, vec!["tests::a_new_refusal".to_owned()]);
        assert_eq!(after.failed, 1);
        assert_eq!(after.passed, 1);
    }

    #[test]
    fn a_red_phase_journals_the_gate_pair_around_the_command_it_ran() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        let _ = gate(&mut run, &ready, &red(), Some(&summary(2, &[])));
        assert_eq!(kinds(&fixture.project), GATED);
        let result = finished(&fixture.project);
        assert_eq!(result.kind, GateKind::Targeted);
        assert!(!result.passed, "a run with a failing test did not pass");
        assert_eq!(result.exit_code, Some(1));
        assert!(
            result.stdout.contains("test result: FAILED"),
            "the row carries the report the command printed: {}",
            result.stdout
        );
    }

    #[test]
    fn a_red_phase_that_fails_nothing_new_does_not_advance() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::an_old_refusal"]));
        let refusal = gate(
            &mut run,
            &ready,
            &red(),
            Some(&summary(1, &["tests::an_old_refusal"])),
        )
        .expect_err("a failure that was already failing proves no new test");
        let Error::Gate { kind, detail } = refusal else {
            panic!("a red phase that proved nothing is a gate refusal, not {refusal:?}");
        };
        assert_eq!(kind, "targeted");
        assert!(
            detail.contains("failing before: `tests::an_old_refusal`")
                && detail.contains("failing after: `tests::an_old_refusal`"),
            "the refusal quotes both lists it compared: {detail}"
        );
        assert_eq!(kinds(&fixture.project), GATED);
    }

    #[test]
    fn a_red_phase_with_no_summary_to_compare_is_refused_before_anything_runs() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(0, &["tests::a_new_refusal"]));
        let refusal = gate(&mut run, &ready, &red(), None)
            .expect_err("red decides by a difference, and there is nothing to differ from");
        assert!(
            matches!(&refusal, Error::NotFound { what } if what.contains("red")),
            "the refusal names the phase it could not decide: {refusal:?}"
        );
        assert!(
            !kinds(&fixture.project).contains(&"GateStarted"),
            "a refusal before the gate ran started no gate: {:?}",
            kinds(&fixture.project)
        );
    }

    #[test]
    fn a_green_phase_with_no_summary_to_compare_is_refused_before_anything_runs() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(2, &[]));
        let refusal = gate(&mut run, &ready, &green(), None)
            .expect_err("green confirms names, and none were given");
        assert!(
            matches!(&refusal, Error::NotFound { what } if what.contains("green")),
            "the refusal names the phase it could not decide: {refusal:?}"
        );
        assert!(
            !kinds(&fixture.project).contains(&"GateStarted"),
            "a refusal before the gate ran started no gate: {:?}",
            kinds(&fixture.project)
        );
    }

    #[test]
    fn a_phase_that_declares_no_gate_names_the_phase_no_command_decides() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(2, &[]));
        let refusal = gate(&mut run, &ready, &ungated(), Some(&summary(1, &[])))
            .expect_err("a phase with no declared gate cannot be decided by this step");
        assert!(
            matches!(&refusal, Error::NotFound { what } if what.contains("green")),
            "the refusal names the phase whose declaration was empty: {refusal:?}"
        );
        assert_eq!(kinds(&fixture.project), PREPARED);
    }

    #[test]
    fn a_phase_whose_gate_no_setting_configures_names_the_key_it_needs() {
        let fixture = Fixture::without_gate_command();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let refusal = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect_err("a targeted gate with no command cannot decide a phase");
        let Error::Config { key, detail } = refusal else {
            panic!("a missing gate command is a configuration refusal, not {refusal:?}");
        };
        assert_eq!(key, "targeted_test_command");
        assert!(
            detail.contains("targeted"),
            "the refusal says which gate has no command: {detail}"
        );
        assert_eq!(kinds(&fixture.project), PREPARED);
    }

    #[test]
    fn a_green_phase_that_leaves_the_named_test_passing_is_told_the_run() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(2, &[]));
        let after = gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect("the test red made fail passes now, and nothing else broke");
        assert_eq!(after.passed, 2);
        assert!(after.failures.is_empty());
        assert_eq!(kinds(&fixture.project), GATED);
    }

    #[test]
    fn a_green_phase_that_breaks_a_passing_test_names_the_regression() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::an_unrelated_test"]));
        let refusal = gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(2, &["tests::a_new_refusal"])),
        )
        .expect_err("a green phase that broke another test is not done");
        let Error::Gate { kind, detail } = refusal else {
            panic!("a regression is a gate refusal, not {refusal:?}");
        };
        assert_eq!(kind, "targeted");
        assert!(
            detail.contains("passing before and failing now: `tests::an_unrelated_test`"),
            "the refusal names the test this phase broke: {detail}"
        );
    }

    #[test]
    fn a_green_phase_whose_named_test_still_fails_names_it() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        let refusal = gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect_err("green ended where red ended");
        let Error::Gate { detail, .. } = refusal else {
            panic!("a still-failing test is a gate refusal, not {refusal:?}");
        };
        assert!(
            detail.contains("expected and still failing: `tests::a_new_refusal`"),
            "the refusal names the test that was to be fixed: {detail}"
        );
    }

    #[test]
    fn a_green_phase_whose_command_refused_is_refused_however_its_report_reads() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(2, &[]));
        fixture.exits_with(1);
        let refusal = gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect_err("a command that refused is not a green phase, whatever it printed");
        let Error::Gate { detail, .. } = refusal else {
            panic!("a refused command is a gate refusal, not {refusal:?}");
        };
        assert!(
            detail.contains("exited with code 1"),
            "the refusal says how the run ended: {detail}"
        );
        assert_eq!(kinds(&fixture.project), GATED);
    }

    #[test]
    fn a_gate_that_reports_no_test_result_is_not_read_as_everything_passing() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let refusal = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect_err("a command that printed no test report proved nothing");
        let Error::Gate { detail, .. } = refusal else {
            panic!("an unreadable run is a gate refusal, not {refusal:?}");
        };
        assert!(
            detail.contains("wrote no test report"),
            "the refusal says what was missing: {detail}"
        );
        assert_eq!(kinds(&fixture.project), GATED);
    }

    #[test]
    fn a_test_report_carried_on_standard_error_is_still_read_as_the_verdict() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        fixture.report_on_stderr();
        let after = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect("a tool that writes its test output on standard error is still reporting");
        assert_eq!(after.failures, vec!["tests::a_new_refusal".to_owned()]);
    }

    #[test]
    fn a_gate_that_cannot_be_started_leaves_its_start_without_a_finish() {
        let fixture = Fixture::with_unspawnable_gate();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let refusal = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect_err("a command that cannot be started decides nothing");
        let Error::Gate { kind, detail } = refusal else {
            panic!("a gate that never started is a gate refusal, not {refusal:?}");
        };
        assert_eq!(kind, "targeted");
        assert!(
            detail.contains("could not be started"),
            "the refusal says the command never ran: {detail}"
        );
        assert_eq!(
            kinds(&fixture.project),
            ["PreflightStarted", "PreflightPassed", "GateStarted"],
            "the start is journaled and the answer that never came is not"
        );
    }

    #[test]
    fn a_red_phase_files_the_command_the_output_and_the_tree_hash() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect("the new failure is what red asked for");
        let filed = fixture.filed("red");
        assert_eq!(filed.len(), 1, "one run files one line");
        let line = &filed[0];
        let script = fixture.script.display().to_string();
        assert_eq!(line["phase"].as_str(), Some("red"));
        assert!(
            Path::new(&script).starts_with(fixture.repo.path()),
            "the stored command names a script inside this fixture's own scratch directory, \
             not one the runner invented: {script}"
        );
        assert_eq!(line["gate"].as_str(), Some("targeted"));
        assert_eq!(line["command"][1].as_str(), Some(script.as_str()));
        assert_eq!(line["base_sha"].as_str(), Some(ready.base_sha.as_str()));
        assert_eq!(line["names"][0].as_str(), Some("tests::a_new_refusal"));
        assert_eq!(line["passed"], Value::Bool(false));
        assert_eq!(line["exit_code"], Value::from(1));
        assert_eq!(line["timed_out"], Value::Bool(false));
        assert!(
            line["stdout"]
                .as_str()
                .is_some_and(|text| text.contains("test result: FAILED")),
            "§9's output is stored, not summarised: {}",
            line["stdout"]
        );
        let tree = line["tree_sha"].as_str().expect("a tree hash is text");
        assert_eq!(tree.len(), 64, "a SHA-256 digest is 64 hex characters");
        assert!(
            tree.chars().all(|digit| digit.is_ascii_hexdigit()),
            "a tree hash is hexadecimal: {tree}"
        );
    }

    #[test]
    fn a_refused_red_phase_files_what_it_ran_as_a_denial() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::an_old_refusal"]));
        let _ = gate(
            &mut run,
            &ready,
            &red(),
            Some(&summary(1, &["tests::an_old_refusal"])),
        )
        .expect_err("nothing failed newly");
        let filed = fixture.filed("red");
        assert_eq!(filed.len(), 1, "a refusal is filed so it can be read");
        assert_eq!(filed[0]["passed"], Value::Bool(false));
        assert_eq!(filed[0]["phase"].as_str(), Some("red"));
        assert!(
            filed[0]["names"].as_array().is_some_and(Vec::is_empty),
            "a refusal named nothing new: {}",
            filed[0]["names"]
        );
    }

    #[test]
    fn the_tree_hash_moves_when_the_checkout_moves() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect("the new failure is what red asked for");
        write_in(
            &ready,
            "tests/a_new_test.rs",
            "#[test] fn a_new_test() {}\n",
        );
        fixture.report(&report(2, &[]));
        gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect("the named test passes now");
        let red = fixture.filed("red");
        let green = fixture.filed("green");
        let before = red[0]["tree_sha"].as_str().expect("red filed a tree hash");
        let after = green[0]["tree_sha"]
            .as_str()
            .expect("green filed a tree hash");
        assert_ne!(
            before, after,
            "the tree green left holds a file red left no trace of"
        );
    }

    /// The hash a phase files has to move when the bytes behind a path move, not only
    /// when the list of paths does. A hash over the names alone would file the same
    /// evidence for a test and for its opposite, which is the whole point of keeping
    /// §9's evidence at all.
    #[test]
    fn the_tree_hash_reads_what_the_changed_path_held() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        write_in(
            &ready,
            "tests/a_new_test.rs",
            "#[test] fn a_new_test() {}\n",
        );
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect("the new failure is what red asked for");
        write_in(&ready, "tests/a_new_test.rs", "// the opposite story\n");
        fixture.report(&report(2, &[]));
        gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect("the named test passes now");
        let held = fixture.filed("red");
        let overwritten = fixture.filed("green");
        let before = held[0]["tree_sha"].as_str().expect("red filed a tree hash");
        let after = overwritten[0]["tree_sha"]
            .as_str()
            .expect("green filed a tree hash");
        assert_ne!(
            before, after,
            "the same path holding other bytes is another tree, and the hash has to say \
             so: {before}"
        );
    }

    /// A path git listed because it is gone is read as gone, not refused: its absence
    /// is the change the phase is being gated over.
    #[test]
    fn a_phase_that_deleted_a_path_hashes_the_absence_and_files_it() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let seed = ready.worktree.join("seed.txt");
        let held = fs::read(&seed).expect("the base commit's seed file is in the checkout");
        fs::remove_file(&seed).expect("a tracked file is removable");
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect("a deletion is a change a phase can be gated over");
        let gone = fixture.filed("red");
        let deleted = gone[0]["tree_sha"]
            .as_str()
            .expect("the deleting phase filed a tree hash");
        fs::write(&seed, held).expect("the seed file is restorable");
        fixture.report(&report(2, &[]));
        gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect("the named test passes now");
        let back = fixture.filed("green");
        let restored = back[0]["tree_sha"]
            .as_str()
            .expect("green filed a tree hash");
        assert_ne!(
            deleted, restored,
            "a path that is gone and that same path holding its bytes are not one tree: \
             {deleted}"
        );
    }

    /// A changed path the filesystem will not let this run read is the run's own
    /// failure to answer, and not a tree in which that path happens to be missing.
    #[test]
    fn a_changed_path_that_cannot_be_read_is_refused_rather_than_hashed_as_absent() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        write_in(
            &ready,
            "tests/a_new_test.rs",
            "#[test] fn a_new_test() {}\n",
        );
        let _sealed = unreadable(&ready.worktree.join("tests/a_new_test.rs"));
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        let refusal = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect_err("a path that cannot be read says nothing about the tree");
        assert!(
            matches!(&refusal, Error::Io(why) if why.kind() == std::io::ErrorKind::PermissionDenied),
            "the filesystem's own refusal is the answer, never a hash over a file that \
             is merely unreadable: {refusal:?}"
        );
    }

    /// A level of the evidence layout that is there and is not a directory is refused
    /// as the rule it broke, rather than written through or cleared out of the way.
    #[test]
    fn a_phases_level_occupied_by_a_file_is_refused_naming_the_level() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let phases = fixture
            .evidence_of("red")
            .parent()
            .expect("an evidence file lives inside a directory")
            .to_path_buf();
        fs::create_dir_all(
            phases
                .parent()
                .expect("the directory holding a phase's files has a parent"),
        )
        .expect("the attempt's evidence directory is creatable");
        fs::write(&phases, "not a directory\n")
            .expect("a file sits where the phases directory belongs");
        fixture.report(&report(1, &["tests::a_new_refusal"]));
        let refusal = gate(&mut run, &ready, &red(), Some(&summary(2, &[])))
            .expect_err("a directory is not made by deleting what someone left there");
        assert!(
            matches!(&refusal, Error::Policy { detail, paths }
                if paths.contains(&phases) && detail.contains("is not a directory")),
            "the refusal names the level that is in the way: {refusal:?}"
        );
        assert_eq!(
            kinds(&fixture.project),
            GATED,
            "the gate ran and was journalled before the filing refused"
        );
    }

    #[test]
    fn a_phase_that_records_no_evidence_files_nothing() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(2, &[]));
        gate(&mut run, &ready, &refactor(), None)
            .expect("the targeted tests stayed green through the cleanup");
        assert!(
            !fixture.evidence_of("refactor").exists(),
            "a phase that declares no evidence of its own files none"
        );
    }

    #[test]
    fn evidence_is_refused_when_its_own_path_is_occupied() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(2, &[]));
        fs::create_dir_all(fixture.evidence_of("green"))
            .expect("a fixture can occupy the evidence path");
        let refusal = gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect_err("evidence cannot be filed through a directory in its place");
        assert!(
            matches!(refusal, Error::Io { .. }),
            "the filesystem refused the write: {refusal:?}"
        );
        assert_eq!(kinds(&fixture.project), GATED);
    }

    #[test]
    fn a_declared_exception_skips_the_red_phase_and_records_itself() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let work = excused();
        let started = summary(3, &["tests::an_old_refusal"]);
        let after = gate_for(&mut run, &ready, &work, &red(), Some(&started))
            .expect("a declared exception needs no new failing test");
        assert_eq!(
            after, started,
            "the skipped phase hands on what it began with"
        );
        let (exception, reason) = claimed(&fixture.project);
        assert_eq!(exception, TddException::Documentation);
        assert_eq!(reason, "Only a doc comment moved.");
        assert_eq!(
            kinds(&fixture.project),
            ["PreflightStarted", "PreflightPassed", "TddExceptionUsed"]
        );
        let phases = for_task(&work, &Config::default())
            .expect("a task may name the protocol it is worked under")
            .phases;
        assert!(
            !phases.iter().any(|spec| spec.phase == Phase::Red),
            "§9's exception removes the red phase from the protocol: {phases:?}"
        );
    }

    #[test]
    fn an_exception_claimed_over_production_code_is_refused_naming_the_path() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        write_in(&ready, "src/main.rs", "fn main() {}\n");
        let work = excused();
        let refusal = gate_for(
            &mut run,
            &ready,
            &work,
            &red(),
            Some(&summary(3, &["tests::an_old_refusal"])),
        )
        .expect_err("an exception is no pardon for what was already written");
        let Error::Policy { paths, .. } = refusal else {
            panic!("a scope violation stays a policy refusal, not {refusal:?}");
        };
        assert!(
            paths.contains(&PathBuf::from("src/main.rs")),
            "the refusal names what broke the scope: {paths:?}"
        );
        assert_eq!(kinds(&fixture.project), PREPARED);
    }

    #[test]
    fn an_exception_claimed_over_tests_only_is_recorded_and_skips_the_gate() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        write_in(
            &ready,
            "tests/a_new_test.rs",
            "#[test] fn a_new_test() {}\n",
        );
        let work = excused();
        let after = gate_for(
            &mut run,
            &ready,
            &work,
            &red(),
            Some(&summary(3, &["tests::an_old_refusal"])),
        )
        .expect("an exception over the paths red was allowed to write is honoured");
        assert_eq!(after.passed, 3);
        assert_eq!(
            kinds(&fixture.project),
            ["PreflightStarted", "PreflightPassed", "TddExceptionUsed"]
        );
    }

    #[test]
    fn a_phase_that_compares_nothing_is_decided_by_its_gate_verdict() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(4, &[]));
        let after = gate(&mut run, &ready, &refactor(), None)
            .expect("the gate passed, so the phase is decided");
        assert_eq!(after.passed, 4);
        assert!(after.failures.is_empty());
    }

    #[test]
    fn a_phase_whose_gate_refused_is_refused_however_much_passed() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        fixture.report(&report(9, &["tests::broken_by_the_cleanup"]));
        let refusal = gate(&mut run, &ready, &refactor(), None)
            .expect_err("a refactor that left a test failing is not decided as done");
        let Error::Gate { kind, detail } = refusal else {
            panic!("a refused gate is a gate refusal, not {refusal:?}");
        };
        assert_eq!(kind, "targeted");
        assert!(
            detail.contains("exited with code 1"),
            "the refusal says how the run ended: {detail}"
        );
    }

    #[test]
    fn the_evidence_a_phase_files_is_redacted_with_the_projects_own_patterns() {
        let fixture = Fixture::with_settings("secret_patterns = [\"s3cret-[a-z]+\"]\n");
        let mut run = fixture.run();
        let ready = prepared(&mut run);
        let mut printed = report(2, &[]);
        printed.push_str("note: s3cret-token printed by the test binary\n");
        fixture.report(&printed);
        gate(
            &mut run,
            &ready,
            &green(),
            Some(&summary(1, &["tests::a_new_refusal"])),
        )
        .expect("the phase is green; the report merely happens to print a secret");
        let text = fs::read_to_string(fixture.evidence_of("green"))
            .expect("the evidence a phase filed is readable");
        assert!(
            !text.contains("s3cret-token"),
            "the configured pattern did not reach the evidence file"
        );
        assert!(
            text.contains(crate::redact::MASK),
            "what it replaced is marked as redacted: {text}"
        );
    }
}

#[cfg(test)]
mod verify_and_publish {
    //! Verification and publication: the only way a task has to
    //! `published_verified`, and the one step of a run whose whole job is to
    //! refuse. The order it follows is VISION.md §10's — commit, then verify the
    //! commit, then publish it — which ADR-0089 records as the reason §10 step 4's
    //! "final verification runs against the exact candidate commit" can be true at
    //! all: a candidate that does not exist yet cannot be verified against.
    //!
    //! Two kinds of evidence run through these tests. The journal rows say what the
    //! run claimed, in order, and a test replays them through
    //! [`crate::Journal::rebuild_state`] whenever it needs to know the rows were
    //! *legal* as well as present — an illegal row is a state machine that cannot
    //! recover, and the projection is the only thing that says so. The gates' own log
    //! file says what actually ran: each of the five commands appends its name to
    //! one file outside every checkout, so "every gate reran from scratch after the
    //! replay" is a count of names in an order rather than an inference from a row.
    //! A row can be written by a step that ran nothing; a line in that file cannot.
    //!
    //! The completion set is configured as five `/bin/sh` calls into one script,
    //! because what this step decides is *which* gates ran, in what order, and what
    //! the run did with their answers. Reading a real suite's counts is
    //! [`crate::parse_cargo`]'s coverage, and running one would make every test here
    //! depend on this workspace compiling at the instant it ran.

    use super::{Prepared, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Error, Event, EventKind, FailureClass, Journal, Phase, Project, Task, TaskId,
        TaskState, git, parse_plan, project_config_path,
    };
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The queue position [`parse_plan`] gives the one-row plan below.
    const TASK: u32 = 1;

    /// The attempt [`Runner::begin_attempt`] numbers for a task that has never been
    /// attempted, which is every fixture here.
    const ATTEMPT: u32 = 1;

    /// The branch every fixture publishes to, and the `mainline_branch` default.
    const BRANCH: &str = "main";

    /// The file the scratch repository's seed commit added: the path a task edits.
    const SEED_FILE: &str = "seed.txt";

    /// The file [`ScratchRepo::diverge`] rewrites on both sides, and the file a
    /// conflicting divergence is therefore about.
    const DIVERGED_FILE: &str = "diverged.txt";

    /// The content every fixture leaves as the work the agent did.
    const WORK: &str = "the work";

    /// The five gates of the completion set, in the order the set runs them.
    const SET: [&str; 5] = ["format", "lint", "build", "verify", "privacy"];

    /// The same set as it runs when a gate before `verify` refused:
    /// [`crate::run_completion_set`] skips what follows a refusal except the
    /// mandatory gate, so a refusal at `build` still spends `verify`.
    const PARTIAL: [&str; 4] = ["format", "lint", "build", "verify"];

    /// The set as it runs when `lint` refused.
    const PARTIAL_FROM_LINT: [&str; 3] = ["format", "lint", "verify"];

    /// The rows the task holds when the step is called: the preflight `prepare`
    /// journalled, the attempt `begin_attempt` opened, and the `Verify` phase the driver
    /// entered before handing the run over — see [`opened`], which explains why that row
    /// has to be there for the journal to replay at all.
    const OPENED: [&str; 4] = [
        "PreflightStarted",
        "PreflightPassed",
        "AttemptStarted",
        "PhaseEntered",
    ];

    /// The rows one completion-set run leaves: a start and a finish for each of the
    /// `gates` gates the profile configured. Counted in gates rather than in runs
    /// because a gate that could not be started leaves its start alone, so the number
    /// a test names is the number of gates it watched.
    fn set_rows(gates: usize) -> Vec<&'static str> {
        (0..gates)
            .flat_map(|_| ["GateStarted", "GateFinished"])
            .collect()
    }

    /// The fixture's repository, its registered project, and the three files the gate
    /// script is driven by: the log of what ran, the base the privacy gate was
    /// handed, and the markers that decide what each gate does.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
        log: PathBuf,
        base_seen: PathBuf,
        markers: PathBuf,
        peer: PathBuf,
    }

    impl Fixture {
        /// A project whose five completion gates all pass.
        fn new() -> Self {
            Self::built("", "")
        }

        /// A project with `extra` beside its base settings — how one test shortens
        /// `gate_timeout_secs` where a gate is made to outlive it.
        fn with_settings(extra: &str) -> Self {
            Self::built(extra, "")
        }

        /// A project whose named gate is configured with a program that is not
        /// there: the one gate refusal that leaves a started row and no finished
        /// one, because there was never a command to finish.
        fn with_unspawnable_gate(gate: &str) -> Self {
            Self::built("", gate)
        }

        /// A project with the five gates configured as calls into one script, with
        /// `extra` added to its settings and one gate — when `broken` names it —
        /// configured with an unspawnable command instead.
        fn built(extra: &str, broken: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            fs::create_dir_all(&state_dir).expect("a state directory is creatable");
            let markers = repo.path().join("markers");
            fs::create_dir_all(&markers).expect("a marker directory is creatable");
            let fixture = Self {
                log: repo.path().join("gates.log"),
                base_seen: repo.path().join("privacy-base"),
                markers,
                peer: repo.path().join("peer-move.sh"),
                project: Project {
                    root: repo.work().to_path_buf(),
                    id: PROJECT_ID.to_owned(),
                    state_dir,
                },
                repo,
            };
            let script = fixture.repo.path().join("gate.sh");
            fs::write(
                &script,
                gate_script(
                    &fixture.log,
                    &fixture.base_seen,
                    &fixture.markers,
                    &fixture.peer,
                ),
            )
            .expect("a gate script is writable");
            fs::write(&fixture.peer, peer_move(&fixture.repo))
                .expect("a peer-move script is writable");
            fs::set_permissions(&fixture.peer, fs::Permissions::from_mode(0o755))
                .expect("a script is made executable");
            let document = settings(&script, extra, broken);
            fs::write(project_config_path(&fixture.project), document)
                .expect("a project settings document is writable");
            fixture
        }

        /// The run this project is configured to have.
        fn run(&self) -> Runner {
            Runner::new(self.project.clone()).expect("a registered, configured project opens a run")
        }

        /// Make the named gate refuse every time it is asked.
        fn refuse(&self, gate: &str) {
            self.touch(&format!("refuse-{gate}"));
        }

        /// Make the named gate pass once and refuse on its second run: how a test
        /// reaches the publication that the second refusal is reached by.
        fn refuse_the_second_time(&self, gate: &str) {
            self.touch(&format!("refuse-{gate}-again"));
        }

        /// Make the named gate outlive any budget a test can configure.
        fn outlive_its_budget(&self, gate: &str) {
            self.touch(&format!("slow-{gate}"));
        }

        /// Move the remote's branch once, during the second run of the format gate.
        /// That window — after a push was refused, before the retry the replay leads
        /// to — is the only place a second refusal can come from, and a second
        /// refusal is the only way to ask whether the retry is bounded.
        fn move_the_remote_again(&self) {
            self.touch("peer-move");
        }

        /// Make the origin move its branch back off whatever it accepts: a
        /// `post-receive` hook resets the branch to the seed commit the moment a push
        /// lands, so the remote ends up holding a commit that is not the one it was
        /// handed.
        ///
        /// This is how [`crate::git::publish`]'s read-back can refuse while its push
        /// and its fetch both succeeded — a server that rewrites what it receives, or
        /// one that simply outraced us. Measured: the push prints its own success, the
        /// fetch reports `forced update`, and the tip read back is the seed. Reaching
        /// that state with a hook rather than with a race is what makes the refusal
        /// testable at all; the alternative is a test that passes or fails depending on
        /// when two processes happened to run.
        ///
        /// The hook goes in a directory of its own, inside the scratch directory, and
        /// the origin is pointed at it by its own local `core.hooksPath`. The
        /// repository's default `hooks/` answers to no one here: the fixture sets
        /// `core.hooksPath` at [`crate::testing`] level so a hook configured for the
        /// machine running the suite cannot run inside it, and writing beside that
        /// setting rather than over it keeps the isolation and the hook both.
        fn move_the_remote_off_what_it_took(&self) {
            let hooks = self.repo.path().join("origin-hooks");
            fs::create_dir_all(&hooks).expect("a hooks directory is creatable");
            let hook = hooks.join("post-receive");
            fs::write(
                &hook,
                format!(
                    "#!/bin/sh\ngit update-ref refs/heads/{BRANCH} '{}'\n",
                    self.repo.seed_sha()
                ),
            )
            .expect("the origin's post-receive hook is writable");
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
                .expect("a hook is made executable");
            let where_hooks_live = hooks.display().to_string();
            git::git(
                self.repo.origin(),
                &["config", "--local", "core.hooksPath", &where_hooks_live],
            )
            .expect("the origin is told where its hooks live");
        }

        fn touch(&self, marker: &str) {
            fs::write(self.markers.join(marker), "").expect("a marker file is writable");
        }

        /// What the gate commands appended, in the order they appended it.
        fn ran(&self) -> Vec<String> {
            let text = fs::read_to_string(&self.log).unwrap_or_default();
            text.lines().map(str::to_owned).collect()
        }

        /// What the privacy gate had in `KTASK_BASE_SHA`, or [`None`] when it never
        /// ran.
        fn base_the_scan_saw(&self) -> Option<String> {
            fs::read_to_string(&self.base_seen).ok()
        }

        /// The tip the origin itself holds, read from the remote rather than from any
        /// checkout's opinion of it.
        fn origin_tip(&self) -> String {
            git::git(self.repo.origin(), &["rev-parse", BRANCH])
                .expect("the origin holds its branch")
        }
    }

    /// The settings the completion set runs under: an adapter this build has, the
    /// five gates as calls into `script`, and a disk floor no test machine breaches.
    /// `baseline_command` is left unset — a baseline nobody configured passes, and a
    /// gate pair for one would sit between the rows these tests count. `broken` names
    /// one gate to configure with a program that cannot be started.
    fn settings(script: &Path, extra: &str, broken: &str) -> String {
        let mut gates = String::new();
        for gate in SET {
            let words: Vec<String> = if gate == broken {
                vec!["/nonexistent/ktask-gate-program".to_owned()]
            } else {
                vec![
                    "/bin/sh".to_owned(),
                    script.display().to_string(),
                    (*gate).to_owned(),
                ]
            };
            let quoted = words
                .iter()
                .map(|word| format!("{word:?}"))
                .collect::<Vec<String>>()
                .join(", ");
            writeln!(&mut gates, "{gate}_command = [{quoted}]")
                .expect("a String is a writer that never refuses");
        }
        format!("provider = \"claude\"\nmin_free_disk_bytes = 1\n{extra}{gates}")
    }

    /// The gate every fixture runs: log that it ran, count how many times it has,
    /// hand the privacy scan nothing beyond its own environment, and refuse or loiter
    /// as the marker files say.
    ///
    /// The markers directory is named once, in a variable every marker test expands
    /// inside double quotes. A path written inside single quotes cannot expand `$name`,
    /// and a gate that cannot see its own marker passes for the wrong reason — which is
    /// how a fixture that looks like it refuses ends up publishing.
    ///
    /// The counter is what makes "refused the second time" and "the remote moved
    /// during the rerun" expressible from outside the step, which is the only way to
    /// test a rerun and a retry bound as behavior rather than as a claim.
    fn gate_script(log: &Path, base_seen: &Path, markers: &Path, peer: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             name=\"$1\"\n\
             m='{}'\n\
             echo \"$name\" >> '{}'\n\
             counter=\"$m/count-$name\"\n\
             seen=0\n\
             if [ -f \"$counter\" ]; then seen=$(cat \"$counter\"); fi\n\
             echo $((seen + 1)) > \"$counter\"\n\
             if [ \"$name\" = privacy ]; then printf '%s' \"$KTASK_BASE_SHA\" > '{}'; fi\n\
             if [ \"$name\" = format ] && [ -f \"$m/peer-move\" ] && [ \"$seen\" -ge 1 ] \\\n\
                 && [ ! -f \"$m/peer-moved\" ]; then\n\
             '{}'\n\
             touch \"$m/peer-moved\"\n\
             fi\n\
             if [ -f \"$m/slow-$name\" ]; then sleep 30; fi\n\
             if [ -f \"$m/refuse-$name\" ]; then echo \"$name refused\" >&2; exit 1; fi\n\
             if [ \"$seen\" -ge 1 ] && [ -f \"$m/refuse-$name-again\" ]; then\n\
             echo \"$name refused the second time\" >&2; exit 1\n\
             fi\n\
             exit 0\n",
            markers.display(),
            log.display(),
            base_seen.display(),
            peer.display(),
        )
    }

    /// A script that moves the origin's branch forward by one commit, from a fresh
    /// clone of it. The clone is what makes the push always a fast-forward: whoever
    /// runs this holds the remote's tip at that moment, which is exactly the state a
    /// task that was too slow to publish finds.
    fn peer_move(repo: &ScratchRepo) -> String {
        format!(
            "#!/bin/sh\n\
             set -e\n\
             clone='{}/peer-clone'\n\
             rm -rf \"$clone\"\n\
             git clone -q '{}' \"$clone\"\n\
             git -C \"$clone\" -c user.name=ktask -c user.email=ktask@example.invalid \\\n\
                 -c commit.gpgsign=false -c core.hooksPath='{}/no-hooks' \\\n\
                 commit -q --allow-empty -m 'the peer moved again'\n\
             git -C \"$clone\" push -q origin '{}'\n",
            repo.path().display(),
            repo.origin().display(),
            repo.path().display(),
            BRANCH,
        )
    }

    /// The queue's one task, spelled as the plan document that would have produced
    /// it — so its id, title and body are the ones a real run would hand this step.
    fn task() -> Task {
        let document = "\
## T093 Runner step: verification and publication

**Outcome:** a task becomes publishable only on mechanical evidence.
**Done-when:** no path reaches publication without a passing completion set.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::verify_and_publish/)'`
**Refs:** VISION.md sections 3, 8 and 10
";
        let parsed = parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        let row = parsed
            .into_iter()
            .next()
            .expect("the fixture plan holds one row");
        assert_eq!(row.id, TaskId::new(TASK), "every fixture works task {TASK}");
        row
    }

    /// Take the queue's task as far as an attempt is open, so the step has a
    /// checkout, a base to be measured against, and an attempt to publish under.
    ///
    /// The driver's own [`crate::EventKind::PhaseEntered`] row is written by hand,
    /// because entering a phase is the driver's move and not this step's —
    /// `verify_and_publish` never writes one, and a test that reached the state by
    /// calling [`Runner::run_phase`] would spend a provider session to get there. It
    /// cannot be left out: `Preflight` refuses a verdict row, so a journal without a
    /// phase entry is a journal [`crate::Journal::rebuild_state`] cannot project, and the
    /// step would look like it had corrupted the log it was only appending to. The phase
    /// is `Verify` because that is the phase a driver is in when it calls this step: it
    /// lands the task in `Verifying`, which is where a verdict row belongs and where the
    /// [`crate::EventKind::VerifyPassed`] that follows moves it to `Publishing`.
    fn opened(run: &mut Runner) -> (Prepared, AttemptId) {
        let row = task();
        let ready = run
            .prepare(&row)
            .expect("nothing in this fixture gives preflight a reason to refuse");
        let attempt = run
            .begin_attempt(&row)
            .expect("the base the preflight recorded opens an attempt");
        assert_eq!(
            attempt,
            AttemptId::new(ATTEMPT),
            "one attempt has been opened"
        );
        run.recorder
            .record(
                Some(row.id),
                EventKind::PhaseEntered {
                    attempt,
                    phase: Phase::Verify,
                },
            )
            .expect("the driver's phase entry is journalled");
        (ready, attempt)
    }

    /// Leave `text` at `path` inside the task's checkout, as a session that wrote
    /// there would have.
    fn write_in(ready: &Prepared, path: &str, text: &str) {
        fs::write(ready.worktree.join(path), text).expect("a checkout file is writable");
    }

    /// Leave the work an ordinary task's session left: one tracked file rewritten.
    fn work(ready: &Prepared) {
        write_in(ready, SEED_FILE, WORK);
    }

    /// Publish what the task's checkout holds.
    fn publish(run: &mut Runner, ready: &Prepared, attempt: AttemptId) -> crate::Result<String> {
        run.verify_and_publish(ready, &task(), attempt)
    }

    /// Run the step and insist it publishes, for the tests whose subject is
    /// something other than the refusal.
    fn published(run: &mut Runner, ready: &Prepared, attempt: AttemptId) -> String {
        publish(run, ready, attempt).expect("nothing here gives verification a reason to refuse")
    }

    /// The commit a checkout stands at.
    fn head(checkout: &Path) -> String {
        git::git(checkout, &["rev-parse", "HEAD"]).expect("a checkout stands on a commit")
    }

    /// The subject line of one commit, which is where a candidate says what it is
    /// for.
    fn subject(checkout: &Path, sha: &str) -> String {
        git::git(checkout, &["log", "-1", "--format=%s", sha]).expect("the candidate is a commit")
    }

    /// The commit one commit was written on top of, which is where a test can see what
    /// a candidate was actually built against.
    fn parent(checkout: &Path, sha: &str) -> String {
        git::git(checkout, &["rev-parse", &format!("{sha}^")])
            .expect("every candidate is written on top of something")
    }

    /// The task's own rows, oldest first, read on a second connection because that is
    /// who asks this question in real life: the TUI, `status`, and the process that
    /// comes after a run that died.
    fn rows(project: &Project) -> Vec<Event> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
    }

    /// The kinds the journal holds for the task, oldest first — not the completion
    /// set's, which belong to no task.
    fn kinds(project: &Project) -> Vec<&'static str> {
        rows(project)
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// Every row in the journal, the task's and the gates' alike, in the order the
    /// journal numbered them — the only reading that can show a gate ran between two
    /// of the task's own rows.
    fn all_kinds(project: &Project) -> Vec<&'static str> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events()
            .expect("the rows this run wrote are readable")
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// Whether the journal holds a row of this kind for the task.
    fn holds(project: &Project, kind: &str) -> bool {
        kinds(project).contains(&kind)
    }

    /// The `VerifyFailed` row the step left, as the attempt and class it named and the
    /// detail it carried.
    fn verdict(project: &Project) -> (AttemptId, FailureClass, String) {
        for row in rows(project) {
            if let EventKind::VerifyFailed {
                attempt,
                class,
                detail,
            } = row.kind
            {
                return (attempt, class, detail);
            }
        }
        panic!("a refused verification leaves the row that says so");
    }

    /// The candidate SHAs the step offered for publication, in the order it offered
    /// them.
    fn candidates(project: &Project) -> Vec<String> {
        let mut offered = Vec::new();
        for row in rows(project) {
            if let EventKind::PublishStarted { candidate_sha, .. } = row.kind {
                offered.push(candidate_sha);
            }
        }
        offered
    }

    /// The state the journal replays to, with an illegal row reported as the failure
    /// it is rather than as a state that was never reached.
    fn replayed(project: &Project) -> Option<TaskState> {
        let mut journal =
            Journal::open_for(project).expect("a registered project's journal is openable");
        journal
            .rebuild_state()
            .expect("every row this step wrote is one the state machine accepts");
        journal
            .get_state(TaskId::new(TASK))
            .expect("the projection is readable")
    }

    /// The gate names one test expects to have run, as the strings the log holds.
    fn ran_words(runs: &[&[&'static str]]) -> Vec<String> {
        runs.iter()
            .flat_map(|names| names.iter().map(|name| (*name).to_owned()))
            .collect()
    }

    /// Where a directory's mode was taken away, put back when dropped so the scratch
    /// directory can still be deleted.
    struct Unreadable {
        path: PathBuf,
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755));
        }
    }

    /// Make `path` unreadable even to its owner, until the returned guard goes away:
    /// the one refusal a fixture cannot be handed by any shorter route, and the
    /// difference between a tree that is clean and a tree that was never read.
    fn unreadable(path: &Path) -> Unreadable {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap_or_else(|why| {
            panic!("`{}` should take mode 000: {why}", path.display());
        });
        Unreadable {
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn a_passing_completion_set_publishes_the_candidate_it_was_verified_against() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let published = published(&mut run, &ready, attempt);

        assert_eq!(
            published,
            head(&ready.worktree),
            "the commit the run published is the commit the checkout stands on"
        );
        assert_eq!(
            published,
            fixture.origin_tip(),
            "the remote holds the very commit the step returned"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&SET]),
            "every gate of the completion set ran, in the set's own order"
        );
        assert_eq!(
            kinds(&fixture.project),
            [
                OPENED.to_vec(),
                vec!["VerifyPassed", "PublishStarted", "PublishVerified"]
            ]
            .concat(),
            "the task's rows are the verdict, the offer and the proof — and nothing else"
        );
        let mut expected = OPENED.to_vec();
        expected.extend(set_rows(SET.len()));
        expected.extend(["VerifyPassed", "PublishStarted", "PublishVerified"]);
        assert_eq!(
            all_kinds(&fixture.project),
            expected,
            "the gate pair for each of the five sits between the attempt and its verdict"
        );
    }

    #[test]
    fn the_candidate_commit_says_which_task_it_delivers() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let published = published(&mut run, &ready, attempt);

        assert_eq!(
            subject(&ready.worktree, &published),
            format!("Task {TASK}: {}", task().title()),
            "the subject line names the task the commit was made for"
        );
    }

    #[test]
    fn a_published_task_replays_to_published_verified_holding_the_candidate() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let published = published(&mut run, &ready, attempt);

        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::PublishedVerified { commit: published }),
            "the journal alone puts the task where publication proved it"
        );
    }

    #[test]
    fn a_refusing_gate_is_a_verification_failure_and_reaches_no_publication() {
        let fixture = Fixture::new();
        fixture.refuse("lint");
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a gate that refused cannot publish the task it gated");

        let Error::Gate { kind, detail } = refusal else {
            panic!("a refused completion gate is a gate refusal, not {refusal:?}");
        };
        assert_eq!(kind, "lint", "the refusal names the gate that refused");
        assert!(
            detail.contains("lint refused") && detail.contains("exited with code 1"),
            "the refusal carries what the gate said and how it ended: {detail}"
        );
        let (attempt_of, class, row) = verdict(&fixture.project);
        assert_eq!(
            attempt_of,
            AttemptId::new(ATTEMPT),
            "the verdict is its attempt's"
        );
        assert_eq!(
            class,
            FailureClass::VerificationFailure,
            "a gate that ran and refused is a verdict about the code"
        );
        assert!(
            row.contains("lint refused"),
            "the row carries the gate's own words: {row}"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&PARTIAL_FROM_LINT]),
            "the set stops spending gates behind a refusal except its mandatory one"
        );
        assert_eq!(
            all_kinds(&fixture.project),
            [
                OPENED.to_vec(),
                set_rows(PARTIAL_FROM_LINT.len()),
                vec!["VerifyFailed"]
            ]
            .concat(),
            "three gate pairs and the refusal: no offer of publication anywhere"
        );
        assert!(
            !holds(&fixture.project, "PublishStarted"),
            "a refused set offers nothing"
        );
        assert_eq!(
            fixture.origin_tip(),
            fixture.repo.seed_sha(),
            "the remote never moved: nothing was published"
        );
        assert_ne!(
            head(&ready.worktree),
            ready.base_sha,
            "the refused task's work is still committed where the next attempt reads it"
        );
    }

    #[test]
    fn a_completion_gate_that_outlives_its_budget_is_an_environment_failure() {
        let fixture = Fixture::with_settings("gate_timeout_secs = 1\n");
        fixture.outlive_its_budget("verify");
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        publish(&mut run, &ready, attempt).expect_err("a gate that never answers decides nothing");

        let (_attempt, class, detail) = verdict(&fixture.project);
        assert_eq!(
            class,
            FailureClass::EnvironmentFailure,
            "a command that ran out of its budget is the machine's answer, not the code's"
        );
        assert!(
            detail.contains("ran out of its 1 s budget"),
            "the row says the budget is what was exceeded: {detail}"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&PARTIAL]),
            "the mandatory gate is the last thing the set spends, and it is the one that loitered"
        );
    }

    #[test]
    fn a_gate_that_cannot_be_started_records_no_verdict() {
        let fixture = Fixture::with_unspawnable_gate("build");
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a gate whose program is not there cannot decide a task");

        let Error::Gate { kind, .. } = refusal else {
            panic!("a gate that could not be started is a gate refusal, not {refusal:?}");
        };
        assert_eq!(kind, "build", "the refusal names the gate that never ran");
        assert!(
            !holds(&fixture.project, "VerifyFailed"),
            "nothing was measured, so there is no verdict to journal"
        );
        assert!(
            !holds(&fixture.project, "VerifyPassed"),
            "and nothing passed either"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&["format", "lint"]]),
            "the set stopped at the gate that could not be started"
        );
        assert_eq!(
            all_kinds(&fixture.project),
            [
                OPENED.to_vec(),
                vec![
                    "GateStarted",
                    "GateFinished",
                    "GateStarted",
                    "GateFinished",
                    "GateStarted"
                ]
            ]
            .concat(),
            "a started row with no finished one after it is what an unstarted gate leaves"
        );
    }

    #[test]
    fn uncommitted_work_at_verification_is_a_policy_failure_naming_every_path() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);
        write_in(&ready, "scratch-notes.md", "a file nobody staged");

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("work nobody committed is not a candidate");

        let Error::Policy { detail, paths } = refusal else {
            panic!("a dirty tree is a policy failure, not {refusal:?}");
        };
        assert!(
            detail.contains("uncommitted work"),
            "the refusal says the rule that was broken: {detail}"
        );
        assert_eq!(
            paths,
            vec![PathBuf::from("scratch-notes.md")],
            "every path that broke the rule is named, and only it"
        );
        let (_attempt, class, row) = verdict(&fixture.project);
        assert_eq!(
            class,
            FailureClass::PolicyFailure,
            "§10 calls a dirty tree at verification a policy failure, and so does the row"
        );
        assert!(
            row.contains("scratch-notes.md"),
            "the row names the file a human has to deal with: {row}"
        );
        assert!(
            fixture.ran().is_empty(),
            "a dirty tree is refused before a single gate is spent"
        );
        assert!(
            !holds(&fixture.project, "PublishStarted"),
            "and nothing is offered"
        );
    }

    #[test]
    fn a_tree_that_cannot_be_read_is_not_reported_as_a_policy_failure() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);
        let unreadable = unreadable(&ready.worktree);

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a tree the run cannot read decides nothing");

        drop(unreadable);
        assert!(
            !matches!(refusal, Error::Policy { .. }),
            "nothing was measured, so no rule was broken: {refusal}"
        );
        assert!(
            matches!(refusal, Error::Git { .. }),
            "git's own refusal is the honest answer, not a policy verdict: {refusal}"
        );
        assert_eq!(
            kinds(&fixture.project),
            OPENED.to_vec(),
            "an unreadable tree leaves no verdict of any kind"
        );
        assert!(fixture.ran().is_empty(), "and it spends no gate either");
    }

    #[test]
    fn a_task_that_changed_nothing_publishes_nothing() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a candidate with no work in it is not a delivery");

        let Error::Policy { detail, .. } = refusal else {
            panic!("nothing to commit is the rule, not a git refusal: {refusal:?}");
        };
        assert!(
            detail.contains("nothing is staged to commit"),
            "the refusal says there was no work to commit: {detail}"
        );
        let (_attempt, class, row) = verdict(&fixture.project);
        assert_eq!(
            class,
            FailureClass::PolicyFailure,
            "a task that delivered nothing is a policy failure, not a passing verification"
        );
        assert!(
            row.contains("nothing is staged to commit"),
            "and the row says so: {row}"
        );
        assert!(
            fixture.ran().is_empty(),
            "no gate is spent proving an empty commit"
        );
        assert!(
            !holds(&fixture.project, "PublishStarted"),
            "and it is never offered"
        );
        assert_eq!(
            head(&ready.worktree),
            ready.base_sha,
            "the checkout still stands where the preflight based it"
        );
    }

    #[test]
    fn the_privacy_gate_is_handed_the_commit_the_candidate_builds_on() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let published = published(&mut run, &ready, attempt);

        assert_eq!(
            fixture.base_the_scan_saw(),
            Some(ready.base_sha.clone()),
            "the scan is told the base the preflight recorded, which is the range it reads"
        );
        assert_ne!(
            fixture.base_the_scan_saw(),
            Some(published),
            "and not the candidate it is scanning the difference to"
        );
    }

    #[test]
    fn a_refused_push_that_replays_cleanly_reruns_every_gate_and_publishes_the_replay() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);
        let divergence = fixture
            .repo
            .diverge(BRANCH)
            .expect("the peer's commit reaches the origin");

        let published = published(&mut run, &ready, attempt);

        let offered = candidates(&fixture.project);
        assert_eq!(
            offered.len(),
            2,
            "the replay was offered as its own candidate"
        );
        assert_eq!(
            published, offered[1],
            "what published is what the replay made"
        );
        assert_eq!(published, fixture.origin_tip(), "and the remote holds it");
        assert_eq!(
            parent(&ready.worktree, &offered[0]),
            ready.base_sha,
            "the first candidate stands on the base the preflight recorded, which is the \
             commit the peer's push moved past"
        );
        assert_ne!(
            offered[0], divergence.remote,
            "the peer's commit is what the remote refused the candidate for"
        );
        assert_eq!(
            parent(&ready.worktree, &published),
            divergence.remote,
            "the replayed candidate stands on the peer's commit: the divergence is \
             history rather than something still to reconcile"
        );
        assert_ne!(offered[0], offered[1], "the replay is a different commit");
        assert_eq!(
            fixture.ran(),
            ran_words(&[&SET, &SET]),
            "every gate ran again from scratch on the replayed commit: no cached evidence"
        );
        let mut expected = OPENED.to_vec();
        expected.extend(set_rows(SET.len()));
        expected.extend(["VerifyPassed", "PublishStarted"]);
        expected.extend(set_rows(SET.len()));
        expected.extend(["VerifyPassed", "PublishStarted", "PublishVerified"]);
        assert_eq!(
            all_kinds(&fixture.project),
            expected,
            "the journal holds two complete gate runs, each closed by its own verdict"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::PublishedVerified { commit: published }),
            "a clean divergence recovers mechanically to the same place a first-time push lands"
        );
    }

    #[test]
    fn a_divergence_that_conflicts_stops_for_a_human_and_names_the_path_it_could_not_resolve() {
        let fixture = Fixture::new();
        fixture
            .repo
            .commit(DIVERGED_FILE, "both sides will want this file")
            .expect("a tracked file the two sides disagree about is committable");
        fixture
            .repo
            .push(BRANCH)
            .expect("and it is on the remote before the task is based on it");
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        write_in(&ready, DIVERGED_FILE, WORK);
        let divergence = fixture
            .repo
            .diverge(BRANCH)
            .expect("the remote moves under the task, rewriting the same file");

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a replay that stops on content cannot be published");

        let Error::Git { stderr, .. } = refusal else {
            panic!("a conflict is git refusing the same branch, not {refusal:?}");
        };
        assert!(
            stderr.contains(DIVERGED_FILE),
            "the refusal names the path the two sides disagree about: {stderr}"
        );
        let class = FailureClass::GitConflict;
        let failed = rows(&fixture.project)
            .iter()
            .find_map(|row| match &row.kind {
                EventKind::TaskFailed {
                    class: found,
                    detail,
                } => Some((*found, detail.clone())),
                _ => None,
            })
            .expect("a conflict stops the task, and says so in the journal");
        assert_eq!(failed.0, class, "§7's class for a conflicting publication");
        assert!(
            failed.1.contains(DIVERGED_FILE),
            "the row names the path a human has to resolve: {}",
            failed.1
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Failed {
                class,
                detail: failed.1
            }),
            "and the task replays as failed rather than as work an agent will be called back to"
        );
        assert!(
            !holds(&fixture.project, "AgentOutput"),
            "no session was started"
        );
        assert!(
            !holds(&fixture.project, "PublishVerified"),
            "the refused candidate was never proved"
        );
        assert_eq!(
            fixture.origin_tip(),
            divergence.remote,
            "the remote is exactly where the peer left it: nothing was forced"
        );
        let dirty = git::git(&ready.worktree, &["status", "--porcelain"])
            .expect("the checkout answers its own status");
        assert!(
            dirty.is_empty(),
            "the conflicted rebase was aborted, not left in the tree: {dirty}"
        );
        let bookkeeping = git::git(
            &ready.worktree,
            &["rev-parse", "--git-path", "rebase-merge"],
        )
        .expect("git says where its rebase bookkeeping would live");
        let inside = PathBuf::from(&bookkeeping);
        let bookkeeping = if inside.is_absolute() {
            inside
        } else {
            ready.worktree.join(inside)
        };
        assert!(
            !bookkeeping.exists(),
            "the checkout was left standing inside an unfinished replay: {bookkeeping:?}"
        );
    }

    #[test]
    fn a_publication_the_remote_refuses_twice_is_not_offered_a_third_time() {
        let fixture = Fixture::new();
        fixture.move_the_remote_again();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);
        fixture
            .repo
            .diverge(BRANCH)
            .expect("the remote moves once before the task tries to publish");

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("the peer moves again while the replay is being gated");

        assert!(
            matches!(refusal, Error::Git { .. }),
            "a remote that will not take the replay is git refusing: {refusal:?}"
        );
        let offered = candidates(&fixture.project);
        assert_eq!(
            offered.len(),
            2,
            "one candidate and one replay were offered, and no third: {offered:?}"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&SET, &SET]),
            "the retry was earned by a complete rerun of the set"
        );
        assert!(
            holds(&fixture.project, "VerifyPassed"),
            "the replay was gated before it was offered again"
        );
        assert!(
            !holds(&fixture.project, "PublishVerified"),
            "and it was not proved, so the task is not published"
        );
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "a second refusal is a refusal, not the last word about the task"
        );
        assert!(
            !offered.contains(&fixture.origin_tip()),
            "what the remote holds is neither candidate: {offered:?}"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(ATTEMPT)
            }),
            "the journal leaves the task where an unfinished push is found: with a commit in the air"
        );
    }

    #[test]
    fn a_refusal_that_cannot_be_repaired_reports_the_fetch_that_refused() {
        let fixture = Fixture::new();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);
        fs::remove_dir_all(fixture.repo.origin()).expect("the remote is taken away mid-run");

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a remote that is gone cannot take a candidate");

        let Error::Git { args, .. } = refusal else {
            panic!("a remote that is gone is git refusing: {refusal:?}");
        };
        assert_eq!(
            args.first().map(String::as_str),
            Some("fetch"),
            "the remote is gone by the time the publication fetches, and that refusal is \
             what comes back: {args:?}"
        );
        assert_eq!(
            candidates(&fixture.project).len(),
            1,
            "one candidate was offered, and the refusal is about the remote, not the gates"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&SET]),
            "the set ran once, before the push"
        );
        assert!(
            !holds(&fixture.project, "VerifyFailed"),
            "the gates passed; saying otherwise would blame the code for a missing remote"
        );
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "and it ends no task"
        );
        assert!(
            !holds(&fixture.project, "PublishVerified"),
            "nothing was proved"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(ATTEMPT)
            }),
            "a run that dies with a push in the air is found there, not back at the gates"
        );
    }

    /// The guard on the one repair this step is allowed to make: a replay answers a
    /// push the remote refused, and nothing else.
    ///
    /// Here git refused nothing but the read-back, so there is no divergence to
    /// replay over — and replaying would be worse than useless, because the remote
    /// holds a tip this checkout is already based on. The step reports what the
    /// fetched tip said and leaves the task in `Publishing`, where recovery finds a
    /// candidate in the air, rather than spending a second completion set on a commit
    /// the remote will move off again.
    #[test]
    fn a_tip_that_is_not_the_candidate_is_reported_rather_than_replayed_onto() {
        let fixture = Fixture::new();
        fixture.move_the_remote_off_what_it_took();
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a remote holding another commit has not been published");

        let Error::Git { args, stderr } = &refusal else {
            panic!("a remote that kept none of the work is git refusing: {refusal:?}");
        };
        assert_eq!(
            args.first().map(String::as_str),
            Some("rev-parse"),
            "the push and the fetch both succeeded, so the refusal is the read-back's \
             own rather than a repair's: {args:?}"
        );
        let offered = candidates(&fixture.project);
        assert_eq!(
            offered.len(),
            1,
            "one candidate was offered, and no second followed a replay: {offered:?}"
        );
        assert_eq!(
            fixture.ran(),
            ran_words(&[&SET]),
            "the set ran once, before the offer: a replay that was never attempted \
             spends no gate on a commit the remote already declined"
        );
        assert!(
            stderr.contains(&offered[0]),
            "the refusal names the candidate the fetched tip is not: {stderr}"
        );
        assert_eq!(
            offered[0],
            head(&ready.worktree),
            "the checkout still stands on the candidate: no replay moved it"
        );
        assert_eq!(
            fixture.origin_tip(),
            fixture.repo.seed_sha(),
            "the remote is back on the seed, holding none of this task's work"
        );
        assert!(
            !holds(&fixture.project, "PublishVerified"),
            "nothing was proved about the candidate"
        );
        assert!(
            !holds(&fixture.project, "VerifyFailed"),
            "the gates passed; the remote is what disagreed, and blaming the code for \
             it would send the next attempt to fix work that is fine"
        );
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "this refusal ends no task: a remote that rewrote what it took is not this \
             step's decision to make"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(ATTEMPT)
            }),
            "the journal leaves the task with the offer it made, which is the only \
             state that can be resumed"
        );
    }

    #[test]
    fn a_replay_that_refuses_the_rerun_is_never_published() {
        let fixture = Fixture::new();
        fixture.refuse_the_second_time("build");
        let mut run = fixture.run();
        let (ready, attempt) = opened(&mut run);
        work(&ready);
        fixture
            .repo
            .diverge(BRANCH)
            .expect("the remote moves, so the first candidate is refused");

        let refusal = publish(&mut run, &ready, attempt)
            .expect_err("a replayed commit that fails a gate cannot be published");

        let Error::Gate { kind, .. } = refusal else {
            panic!("the rerun refused on a gate, which is a gate refusal: {refusal:?}");
        };
        assert_eq!(kind, "build", "the gate that refused the replay is named");
        assert_eq!(
            fixture.ran(),
            ran_words(&[&SET, &PARTIAL]),
            "the set ran in full, and again until the gate that refused the replay"
        );
        assert_eq!(
            candidates(&fixture.project).len(),
            1,
            "the replayed commit was never offered, because it was never proved"
        );
        assert!(
            !holds(&fixture.project, "VerifyFailed"),
            "the state a rejected push leaves has no room for a verification verdict, and an \
             illegal row is worse than none"
        );
        assert!(
            !holds(&fixture.project, "PublishVerified"),
            "nothing was published"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(ATTEMPT)
            }),
            "the task stays where a refused push left it, for whoever reads the dangling pair"
        );
    }
}

#[cfg(test)]
mod run_task {
    //! One task driven end to end: the steps in the order that makes a task
    //! finish, and what a run that stops partway leaves behind.
    //!
    //! Named after the method it tests, the way `mod new`, `mod prepare`,
    //! `mod report`, `mod run_phase`, `mod gate_phase` and
    //! `mod verify_and_publish` are named after theirs, because the task that
    //! asked for this one fixed `test(/runner::run_task/)` as its Verify command.
    //!
    //! Two kinds of evidence run through these tests, for the reasons the two
    //! modules before this one gives them. The journal's own rows say what the run
    //! claimed, and every one is replayed through [`crate::Journal::rebuild_state`]
    //! so that "complete" means *legal as well as present* — a row the machine
    //! refuses is a journal no later run can recover from. The gate script's log
    //! says which commands actually ran, in order: a row can be written by a step
    //! that ran nothing, and a line in that log cannot.
    //!
    //! The targeted gate prints the next report in a plan each test writes — one
    //! file per run, in order. That is what lets a `tdd` run answer red's session
    //! with a new failure and green's with that same name passing, and it is also
    //! what makes a phase gated too often or too seldom loud: a driver that ran one
    //! gate too many runs out of planned reports and is refused, rather than
    //! repeating the last one and looking correct.
    //!
    //! The second thing these tests hold the driver to is what a refusal costs. A
    //! run that stopped has given the repository lock back whatever happened, and it
    //! keeps a checkout whose tree holds work — the session's uncommitted work is the
    //! evidence a later attempt is told to read (VISION.md §7) — while a checkout
    //! that holds nothing is removed rather than left for a human to find.
    //!
    //! Those tests are about one refusal, so they are run with
    //! [`Fixture::single_attempt`], which sets the project's own ceiling on repairs to
    //! zero. §7's remediation has a module of its own; leaving a repair budget here
    //! would only make every one of these runs start a second session and have its
    //! scenario run out of steps on the way to the ending being examined.

    use super::{Machine, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Error, Event, EventKind, FailureClass, Journal, Phase, Project, Task, TaskId,
        TaskState, evidence_dir, git, lock, parse_plan, project_config_path,
    };
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The queue position [`parse_plan`] gives the one-row plan below, and so the
    /// task every run here is driven for.
    const TASK: u32 = 1;

    /// The attempt a task that has never been attempted is opened at.
    const ATTEMPT: u32 = 1;

    /// The file the scratch repository's seed commit tracks — the only path a
    /// scripted session can change and the run can then commit, because
    /// [`crate::git::commit_all`] stages tracked paths and no one stages the rest.
    const SEED_FILE: &str = "seed.txt";

    /// What a session that finished leaves in the report the prompt named.
    const DONE: &str = "KTASK_RESULT: DONE\nSummary: every step the task had is worked.\n";

    /// What a session that stopped short leaves there instead.
    const FAILED: &str =
        "KTASK_RESULT: FAILED\nSummary: what is left is more than the remainder.\n";

    /// What every scripted session prints, as two lines so a row per line is
    /// observable rather than assumed.
    const PRINTED: &str = "reading the seed\nwriting the fix\n";

    /// The test name a `tdd` run's red phase is expected to break and green is
    /// expected to leave passing.
    const NEW_TEST: &str = "the_new_test";

    /// A disk floor no machine running this suite clears, for the one test whose
    /// subject is a preflight that refuses.
    const TOO_BIG_A_FLOOR: u64 = 1_000_000_000_000_000;

    /// The rows a run that worked one phase and published it leaves for the task,
    /// oldest first. The completion set's own pairs are absent because
    /// [`crate::run_completion_set`] attributes them to no task.
    const DIRECT_DONE: [&str; 14] = [
        "PreflightStarted",
        "PreflightPassed",
        "AttemptStarted",
        "PhaseEntered",
        "AgentOutput",
        "AgentOutput",
        "AttemptFinished",
        "GateStarted",
        "GateFinished",
        "PhaseEntered",
        "VerifyPassed",
        "PublishStarted",
        "PublishVerified",
        "TaskDone",
    ];

    /// The rows a run leaves when a phase is refused for what it wrote: the pair its
    /// baseline gate left — which sits before the phase's entry because it is the
    /// measurement the phase starts from, not an answer it gave — then the entry and
    /// the session's own rows, then the one row that ends the task; no gate, verdict or
    /// publication between them.
    const REFUSED_RED: [&str; 10] = [
        "PreflightStarted",
        "PreflightPassed",
        "AttemptStarted",
        "GateStarted",
        "GateFinished",
        "PhaseEntered",
        "AgentOutput",
        "AgentOutput",
        "AttemptFinished",
        "TaskFailed",
    ];

    /// The rows a run leaves when its one session ended without a report: the
    /// session's own story, then the row that ends the task, and between them no gate,
    /// no verdict, no publication.
    const UNREPORTED: [&str; 8] = [
        "PreflightStarted",
        "PreflightPassed",
        "AttemptStarted",
        "PhaseEntered",
        "AgentOutput",
        "AgentOutput",
        "AttemptFinished",
        "TaskFailed",
    ];

    /// The sentence [`crate::should_continue`] answers with when a project that allows
    /// no remediation has been refused once. The row that ends each refusal below
    /// carries it, because for these runs the spent bound *is* why the run stopped.
    const SPENT: &str = "attempts 1 past the 0 bound";

    /// The settings a whole run is opened with: the scripted adapter and its
    /// scenario file, the phase gate and the completion gate as calls into one
    /// script, `floor` as the disk floor, and `extra` appended.
    ///
    /// One script answers both gates because what a driver's tests turn on is which
    /// commands ran in what order, and two scripts would let a test pass without
    /// saying which of them a step called.
    fn settings(scenario: &Path, script: &Path, floor: u64, extra: &str) -> String {
        format!(
            "provider = \"dummy\"\n\
             dummy_scenario_path = \"{}\"\n\
             min_free_disk_bytes = {floor}\n\
             targeted_test_command = [\"/bin/sh\", \"{}\", \"targeted\"]\n\
             verify_command = [\"/bin/sh\", \"{}\", \"verify\"]\n\
             {extra}",
            scenario.display(),
            script.display(),
            script.display()
        )
    }

    /// The gate script: log the name it was called under, and answer as that gate.
    ///
    /// The completion gate passes unless a marker says it refuses, because which
    /// completion gate refused is not what a driver decides — `mod
    /// verify_and_publish` covers the set itself. The targeted gate prints the next
    /// planned report and exits the way that report reads, exactly as the script in
    /// `mod gate_phase` does, and when the plan has no report left it refuses
    /// without one: a run that gated a phase nobody planned for has to be heard
    /// about, and a repeated report would hide it.
    fn gate_script(log: &Path, markers: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             name=\"$1\"\n\
             m='{}'\n\
             echo \"$name\" >> '{}'\n\
             if [ \"$name\" = verify ]; then\n\
             if [ -f \"$m/refuse-verify\" ]; then echo 'verify refused' >&2; exit 1; fi\n\
             exit 0\n\
             fi\n\
             seen=0\n\
             if [ -f \"$m/targeted-seen\" ]; then seen=$(cat \"$m/targeted-seen\"); fi\n\
             echo $((seen + 1)) > \"$m/targeted-seen\"\n\
             report=\"$m/report-$((seen + 1)).txt\"\n\
             if [ ! -f \"$report\" ]; then echo 'targeted: the plan holds no further report' >&2; exit 2; fi\n\
             cat \"$report\"\n\
             if grep -q '^test result: FAILED' \"$report\"; then exit 1; fi\n\
             exit 0\n",
            markers.display(),
            log.display()
        )
    }

    /// A registered project, the scenario its adapter replays, the script its two
    /// gates run, and the log and marker directory that script is driven by.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
        scenario: PathBuf,
        markers: PathBuf,
        log: PathBuf,
        config_home: PathBuf,
    }

    impl Fixture {
        /// A project whose gates pass and whose disk floor is one byte.
        fn new() -> Self {
            Self::built(1, "")
        }

        /// As [`Fixture::new`], with the `tdd` red phase's write scope drawn on the
        /// file a scripted session is able to change.
        ///
        /// `seed.txt` is the only tracked path the scratch repository has, and a red
        /// phase may write test paths only; a project that names its test paths
        /// somewhere the seed commit never put a file could not have a red phase
        /// whose work the run could then commit and publish.
        fn tdd_ground() -> Self {
            Self::built(1, "test_globs = [\"seed.txt\"]\n")
        }

        /// As [`Fixture::new`], on a filesystem the preflight will call too full.
        fn short_on_disk() -> Self {
            Self::built(TOO_BIG_A_FLOOR, "")
        }

        /// As [`Fixture::new`], with the project allowing its refusals no repair.
        ///
        /// `max_remediation_attempts = 0` is the configuration's own way of saying so,
        /// and it is how a test looks at one refusal's ending on its own — §7's repair
        /// is `mod remediation`'s subject, with a scenario shaped for it.
        fn single_attempt() -> Self {
            Self::built(1, "max_remediation_attempts = 0\n")
        }

        /// A project configured with `floor` as its disk floor and `extra` appended
        /// to its settings document.
        fn built(floor: u64, extra: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            let scenario = repo.path().join("scenario.toml");
            let script = repo.path().join("gate.sh");
            let log = repo.path().join("gate.log");
            let markers = repo.path().join("markers");
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::create_dir_all(&markers).expect("a marker directory is creatable");
            fs::write(&script, gate_script(&log, &markers)).expect("a gate script is writable");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
                .expect("a gate script is made executable");
            let config_home = repo.path().join("config-home");
            fs::write(
                project_config_path(&project),
                settings(&scenario, &script, floor, extra),
            )
            .expect("a project settings document is writable");
            Self {
                repo,
                project,
                scenario,
                markers,
                log,
                config_home,
            }
        }

        /// The script the adapter replays, written before the run is opened:
        /// [`crate::provider::build`] reads the file once, so a scenario written
        /// after the run was built would be a scenario the run never saw.
        fn script(&self, document: &str) {
            fs::write(&self.scenario, document).expect("a scenario document is writable");
        }

        /// The reports the targeted gate prints, one per run, in order.
        fn plan(&self, reports: &[String]) {
            for (index, text) in reports.iter().enumerate() {
                let number = index + 1;
                fs::write(self.markers.join(format!("report-{number}.txt")), text)
                    .expect("a planned report is writable");
            }
        }

        /// Make the completion gate refuse.
        fn refuse_verify(&self) {
            fs::write(self.markers.join("refuse-verify"), "").expect("a marker is writable");
        }

        /// Leave `words` at the path the prompt names, as a session that reported
        /// would have. Written by hand rather than through [`Runner::prepare_report`]
        /// because the run under test is what calls that step.
        fn report(&self, attempt: u32, words: &str) {
            let path = self
                .project
                .state_dir
                .join("attempts")
                .join(TASK.to_string())
                .join(attempt.to_string())
                .join("agent-report.md");
            fs::create_dir_all(
                path.parent()
                    .expect("a report is spelled below an attempt directory"),
            )
            .expect("an attempt's report directory is creatable");
            fs::write(&path, words).expect("a report is writable");
        }

        /// The run this project is configured to have.
        fn run(&self) -> Runner {
            Runner::new(self.project.clone()).expect("a registered, configured project opens a run")
        }

        /// The environment a phase's prompt is read from: a configuration home of
        /// the fixture's own, so no test aims the prompt library at the machine
        /// running it.
        fn env(&self) -> impl Fn(&str) -> Option<String> {
            let home = self.config_home.clone();
            move |key: &str| (key == "XDG_CONFIG_HOME").then(|| home.display().to_string())
        }

        /// The gate names that actually ran, in the order they ran.
        fn ran(&self) -> Vec<String> {
            let text = fs::read_to_string(&self.log).unwrap_or_default();
            text.lines().map(str::to_owned).collect()
        }

        /// Every checkout the repository registers, the user's own included.
        fn checkouts(&self) -> Vec<git::Worktree> {
            git::list_worktrees(&self.project.root).expect("the repository answers what it holds")
        }

        /// The task's own checkout: the one registered checkout that is not the
        /// project's root.
        ///
        /// Read out of what git registers rather than rebuilt from the directory
        /// shape [`crate::git`] gives a managed checkout, because the question this
        /// answers is "what is left in the repository after the run", and a path the
        /// test assembled itself would agree with a run that put its work somewhere
        /// else entirely.
        fn checkout(&self) -> PathBuf {
            self.checkouts()
                .into_iter()
                .map(|entry| entry.path)
                .find(|path| path != &self.project.root)
                .expect("a run that kept a checkout left one the repository registers")
        }

        /// Where one attempt's evidence for `phase` is spelled to live.
        fn evidence(&self, phase: &str) -> PathBuf {
            evidence_dir(&self.project, TaskId::new(TASK), AttemptId::new(ATTEMPT))
                .join("phases")
                .join(format!("{phase}.jsonl"))
        }

        /// The tip the origin itself holds.
        fn origin_tip(&self) -> String {
            git::git(self.repo.origin(), &["rev-parse", "main"])
                .expect("the origin holds its branch")
        }
    }

    /// The queue's one task, worked by the project's default protocol.
    fn task() -> Task {
        let document = "\
## T094 Runner step: drive one task end to end

**Outcome:** the steps compose into a task that completes.
**Done-when:** a dummy run reaches done and leaves no worktree or lock behind.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::run_task/)'`
**Refs:** VISION.md section 6
";
        let parsed = parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        let row = parsed
            .into_iter()
            .next()
            .expect("the fixture plan holds one row");
        assert_eq!(row.id, TaskId::new(TASK), "every fixture works task {TASK}");
        row
    }

    /// The same row, worked by `tdd`: red, green, refactor, then the ending.
    fn tdd_task() -> Task {
        let mut row = task();
        row.protocol = Some("tdd".to_owned());
        row
    }

    /// A scenario document's string, escaped the way TOML wants the breaks agent
    /// text is full of.
    fn toml_text(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    /// A scenario of one successful session per entry in `writes`, each printing
    /// [`PRINTED`] and leaving its own files in the checkout it runs in. An empty
    /// entry is a session that touches nothing.
    fn sessions(writes: &[Vec<(&str, &str)>]) -> String {
        let mut document = String::new();
        for files in writes {
            writeln!(
                document,
                "[[steps]]\non_task = {TASK}\noutcome = \"success\"\nstdout = \"{}\"\n",
                toml_text(PRINTED)
            )
            .expect("a String always has room for what is written into it");
            if !files.is_empty() {
                document.push_str("[steps.files]\n");
                for (path, contents) in files {
                    writeln!(document, "\"{path}\" = \"{}\"", toml_text(contents))
                        .expect("a String always has room for what is written into it");
                }
            }
            document.push('\n');
        }
        document
    }

    /// A cargo-shaped report: one test binary that opened, named its failures under
    /// the `failures:` block, and answered with the result line.
    fn report(passed: u32, failing: &[&str]) -> String {
        let failed = u32::try_from(failing.len()).unwrap_or(u32::MAX);
        let mut text = format!("running {} tests\n", passed + failed);
        for name in failing {
            writeln!(&mut text, "test {name} ... FAILED")
                .expect("a String is a writer that never refuses");
        }
        if !failing.is_empty() {
            text.push_str("failures:\n");
            for name in failing {
                writeln!(&mut text, "    {name}").expect("a String never refuses a write");
            }
        }
        let verdict = if failing.is_empty() { "ok" } else { "FAILED" };
        write!(
            &mut text,
            "\ntest result: {verdict}. {passed} passed; {failed} failed; 0 ignored; 0 measured; \
             0 filtered out; finished in 0.00s\n"
        )
        .expect("a String never refuses a write");
        text
    }

    /// The task's own rows, oldest first, read on a second connection because that
    /// is who asks this question in real life.
    fn rows(project: &Project) -> Vec<Event> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
    }

    /// The kinds the journal holds for the task, oldest first.
    fn kinds(project: &Project) -> Vec<&'static str> {
        rows(project)
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// Whether the journal holds a row of this kind for the task.
    fn holds(project: &Project, kind: &str) -> bool {
        kinds(project).contains(&kind)
    }

    /// The task's `TaskFailed` row as the state machine folds it: the class the refusal
    /// was sorted into and the detail the row carries, or `None` when the task was
    /// never refused.
    fn failure(project: &Project) -> Option<(FailureClass, String)> {
        rows(project).iter().find_map(|row| match &row.kind {
            EventKind::TaskFailed { class, detail } => Some((*class, detail.clone())),
            _ => None,
        })
    }

    /// The phases the journal says were entered, in order — the reading that says
    /// which phases a protocol actually worked.
    fn entered(project: &Project) -> Vec<Phase> {
        let mut phases = Vec::new();
        for row in rows(project) {
            if let EventKind::PhaseEntered { phase, .. } = row.kind {
                phases.push(phase);
            }
        }
        phases
    }

    /// The commit the `TaskDone` row closed the task on.
    fn closed_on(project: &Project) -> String {
        for row in rows(project) {
            if let EventKind::TaskDone { commit } = row.kind {
                return commit;
            }
        }
        panic!("a finished task is closed on the commit it was published as");
    }

    /// The state the journal replays to, with an illegal row reported as the failure
    /// it is rather than as a state that was never reached.
    fn replayed(project: &Project) -> Option<TaskState> {
        let mut journal =
            Journal::open_for(project).expect("a registered project's journal is openable");
        journal
            .rebuild_state()
            .expect("every row this run wrote is one the state machine accepts");
        journal
            .get_state(TaskId::new(TASK))
            .expect("the projection is readable")
    }

    /// Take the project's lock from a test, as an outsider would, and give it back.
    fn lock_is_free(project: &Project) -> bool {
        let Ok(held) = lock::acquire(&project.state_dir, Duration::ZERO) else {
            return false;
        };
        held.release().expect("a lock this test took is given back");
        true
    }

    /// Drive the queue's task to the end, with the prompt read from the fixture's
    /// own configuration home.
    fn finish(fixture: &Fixture, run: &mut Runner, work: &Task) -> TaskState {
        run.run_task_with(&fixture.env(), work, &Machine)
            .expect("nothing here gives a step a reason to refuse")
    }

    /// Drive the queue's task and hand back the refusal it stopped on.
    fn refusal(fixture: &Fixture, run: &mut Runner, work: &Task) -> Error {
        match run.run_task_with(&fixture.env(), work, &Machine) {
            Err(why) => why,
            Ok(state) => panic!("this fixture is built to stop the run, and it reached {state:?}"),
        }
    }

    #[test]
    fn a_direct_run_ends_at_done_with_every_step_journaled_in_order() {
        let fixture = Fixture::new();
        fixture.script(&sessions(&[vec![(SEED_FILE, "the work")]]));
        fixture.plan(&[report(1, &[])]);
        fixture.report(ATTEMPT, DONE);
        let mut run = fixture.run();

        let state = finish(&fixture, &mut run, &task());

        assert_eq!(
            state,
            TaskState::Done,
            "the run's answer is the state its own journal folds to"
        );
        assert_eq!(
            kinds(&fixture.project),
            DIRECT_DONE,
            "preflight, attempt, the phase and its gate, the verify phase and its verdict, \
             the publication and its read-back, and the row that closes the task"
        );
        assert_eq!(
            entered(&fixture.project),
            [Phase::Implement, Phase::Verify],
            "the work phase the protocol declares, then the ending — and no `Publish` entry, \
             which no state accepts"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "verify"],
            "the phase's own gate, then the completion set — once each"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Done),
            "a replay of every row lands where the run says it did: the journal is complete"
        );
        assert_eq!(
            fixture.origin_tip(),
            closed_on(&fixture.project),
            "the commit the task was closed on is the commit the remote was read back holding"
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "the task's checkout is removed once its work is published: {:?}",
            fixture.checkouts()
        );
        assert!(
            lock_is_free(&fixture.project),
            "the repository lock is given back at the end of the run"
        );
    }

    #[test]
    fn a_tdd_run_works_every_phase_its_protocol_declares_in_order() {
        let fixture = Fixture::tdd_ground();
        fixture.script(&sessions(&[
            vec![(SEED_FILE, "the test, failing")],
            vec![(SEED_FILE, "the fix")],
            vec![],
        ]));
        fixture.plan(&[
            report(1, &[]),
            report(1, &[NEW_TEST]),
            report(2, &[]),
            report(1, &[]),
        ]);
        fixture.report(ATTEMPT, DONE);
        let mut run = fixture.run();

        let state = finish(&fixture, &mut run, &tdd_task());

        assert_eq!(
            state,
            TaskState::Done,
            "a red phase that broke a new test and a green one that fixed it finish"
        );
        assert_eq!(
            entered(&fixture.project),
            [Phase::Red, Phase::Green, Phase::Refactor, Phase::Verify],
            "one entry per phase the protocol declares, in the order it declares them"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "targeted", "targeted", "targeted", "verify"],
            "red's gate ran before its session, so the new failure was new, and once after \
             each of the three phases; the ending ran the completion set"
        );
        assert!(
            fixture.evidence("red").is_file() && fixture.evidence("green").is_file(),
            "the two phases that declare §9's evidence filed it"
        );
        assert!(
            !fixture.evidence("refactor").is_file(),
            "the phase that declares none filed none"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Done),
            "three phases' rows and one ending replay to done"
        );
        assert_eq!(fixture.checkouts().len(), 1, "and the checkout goes");
        assert!(lock_is_free(&fixture.project), "and the lock comes back");
    }

    #[test]
    fn a_refused_completion_gate_ends_the_task_and_holds_nothing_back() {
        let fixture = Fixture::single_attempt();
        fixture.script(&sessions(&[vec![(SEED_FILE, "the work")]]));
        fixture.plan(&[report(1, &[])]);
        fixture.report(ATTEMPT, DONE);
        fixture.refuse_verify();
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::Gate { kind, .. } if kind == "verify"),
            "the refusing gate's own refusal comes back unchanged, and it was {why}"
        );
        assert_eq!(
            failure(&fixture.project),
            Some((FailureClass::VerificationFailure, SPENT.to_owned())),
            "a refusal whose project has no repair left to spend is classified and ends the \
             task, rather than leaving the task parked mid-run"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Failed {
                class: FailureClass::VerificationFailure,
                detail: SPENT.to_owned(),
            }),
            "the task ends where the refused set left it, and every row to get there is legal"
        );
        assert!(
            !holds(&fixture.project, "SelfHealingReport"),
            "nothing recovered, so there is no account for a recovery to give"
        );
        assert!(
            !holds(&fixture.project, "PublishStarted") && !holds(&fixture.project, "TaskDone"),
            "a candidate no completion set proved was never offered"
        );
        assert_eq!(
            fixture.origin_tip(),
            fixture.repo.seed_sha(),
            "nothing reached the remote"
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "the checkout held nothing the run had not committed, so it is removed: {:?}",
            fixture.checkouts()
        );
        assert!(
            lock_is_free(&fixture.project),
            "a run that refused gives the lock back too — the next attempt needs it"
        );
    }

    #[test]
    fn a_refusal_that_left_work_behind_keeps_the_checkout_and_gives_the_lock_back() {
        let fixture = Fixture::single_attempt();
        fixture.script(&sessions(&[vec![(
            "notes.md",
            "outside a red phase's scope",
        )]]));
        fixture.plan(&[report(1, &[])]);
        fixture.report(ATTEMPT, DONE);
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &tdd_task());

        assert!(
            matches!(&why, Error::Policy { paths, .. } if paths.contains(&PathBuf::from("notes.md"))),
            "the scope refusal names the path the session wrote, and it was {why}"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted"],
            "the run stopped at the phase, not at its gate: only the baseline run before the \
             session reached a command"
        );
        assert_eq!(
            fixture.checkouts().len(),
            2,
            "the checkout that holds the session's work is kept for the attempt that reads \
             it (VISION.md §7): {:?}",
            fixture.checkouts()
        );
        assert_eq!(
            fs::read_to_string(fixture.checkout().join("notes.md"))
                .expect("the work is where the next attempt is told to look"),
            "outside a red phase's scope",
            "unchanged, and not quietly discarded on the way out"
        );
        assert!(
            lock_is_free(&fixture.project),
            "keeping the checkout is not keeping the lock: another run can start the moment \
             this one stopped"
        );
        assert_eq!(
            kinds(&fixture.project),
            REFUSED_RED,
            "the baseline the red phase was measured against, the entry, the session's own \
             rows — and no gate, no verdict, no publication, because the phase was refused \
             for what it wrote before anything was asked of it; only the row that ends the \
             task comes after"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Failed {
                class: FailureClass::PolicyFailure,
                detail: SPENT.to_owned(),
            }),
            "the scope refusal is sorted into §7's own class for a forbidden file, and the \
             ending row it earns is one the machine accepts where the run left the task"
        );
    }

    #[test]
    fn a_session_that_left_no_report_stops_the_run_before_the_ending() {
        let fixture = Fixture::single_attempt();
        fixture.script(&sessions(&[vec![]]));
        fixture.plan(&[report(1, &[])]);
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::NotFound { what } if what.contains("agent-report.md")),
            "an attempt that accounted for nothing is refused by the path its report should \
             have been at, and it was {why}"
        );
        assert_eq!(
            kinds(&fixture.project),
            UNREPORTED,
            "the session's own rows, then the row that ends the task, and no gate, no \
             verdict, no publication between them"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Failed {
                class: FailureClass::EnvironmentFailure,
                detail: SPENT.to_owned(),
            }),
            "an attempt that accounted for nothing is the supervisor's own fault, and the \
             run ends rather than leaving the task mid-phase"
        );
        assert!(
            fixture.ran().is_empty(),
            "nothing was gated: a phase that never reported has nothing to have proved, and \
             it was {:?}",
            fixture.ran()
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "the checkout held nothing, so it is removed rather than left: {:?}",
            fixture.checkouts()
        );
        assert!(lock_is_free(&fixture.project), "and the lock comes back");
    }

    #[test]
    fn a_session_that_said_it_stopped_short_is_not_carried_to_publication() {
        let fixture = Fixture::single_attempt();
        fixture.script(&sessions(&[vec![(SEED_FILE, "half of the work")]]));
        fixture.plan(&[report(1, &[])]);
        fixture.report(ATTEMPT, FAILED);
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::NotFound { what } if what.contains("FAILED")),
            "the refusal names the claim the phase ended on, and it was {why}"
        );
        assert!(
            fixture.ran().is_empty(),
            "its gate never ran: a phase whose own session says it stopped short has nothing \
             to have proved, and it was {:?}",
            fixture.ran()
        );
        assert!(
            !holds(&fixture.project, "PublishStarted") && !holds(&fixture.project, "TaskDone"),
            "a task its own session calls unfinished is not published and not closed"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Failed {
                class: FailureClass::EnvironmentFailure,
                detail: SPENT.to_owned(),
            }),
            "the journal says honestly where the run stopped, and it stops: the refusal is \
             classified and the task ends rather than being carried to a publication"
        );
        assert_eq!(
            fixture.checkouts().len(),
            2,
            "and the half-finished work is kept with it: {:?}",
            fixture.checkouts()
        );
        assert!(lock_is_free(&fixture.project), "while the lock comes back");
    }

    #[test]
    fn a_refused_preflight_leaves_nothing_to_clean_up() {
        let fixture = Fixture::short_on_disk();
        fixture.script(&sessions(&[vec![]]));
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::NotFound { what } if what.contains("preflight")),
            "the refusal is the preflight's own, naming the check that refused, and it was \
             {why}"
        );
        assert_eq!(
            kinds(&fixture.project),
            ["PreflightStarted", "PreflightFailed"],
            "the verdict the checks earned, and nothing after it"
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "no checkout is cut for a task that was refused before it began: {:?}",
            fixture.checkouts()
        );
        assert!(
            lock_is_free(&fixture.project),
            "and no lock is held on its behalf: {:?}",
            lock::lock_path(&fixture.project.state_dir)
        );
    }
}

#[cfg(test)]
mod remediation {
    //! One failed attempt, retried once, from a session that never existed before.
    //!
    //! Named after what it tests the way `mod new`, `mod prepare`, `mod report`,
    //! `mod run_phase`, `mod gate_phase`, `mod verify_and_publish` and
    //! `mod run_task` are named after theirs, because the task that asked for this
    //! one fixed `test(/runner::/)` as its Verify command.
    //!
    //! §7's requirements are all about what a repair is *made of*, and none of them
    //! are visible in a value the runner returns: a fresh session rather than a
    //! resumed one, the bundle it is seeded with, the gates run again from scratch,
    //! the bounds and the breaker that stop the whole thing, and the attempt records
    //! that say two attempts happened. So every assertion here is read out of a
    //! record some third party keeps — the gate script's log of which commands
    //! actually ran, the journal replayed through [`crate::Journal::rebuild_state`]
    //! so an illegal row cannot pass, the evidence directory's attempt count, and
    //! the prompts an adapter recorded verbatim. Nothing asserts what the runner
    //! remembers in memory, because a memory a rerun cannot check is exactly what
    //! §7 refuses to rely on.

    use super::{Machine, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Bus, Capabilities, Error, Event, EventKind, Invocation, Journal, Outcome,
        PauseReason, Phase, Project, Provider, Result, Task, TaskId, TaskState, Usage, UsageSource,
        apply, evidence_dir, git, lock, parse_plan, project_config_path, provider, read_evidence,
        report_path,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Duration;

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The queue position [`parse_plan`] gives the one-row plan below, and so the
    /// task every run here is driven for.
    const TASK: u32 = 1;

    /// The attempt a task that has never been attempted is opened at.
    const FIRST: u32 = 1;

    /// The attempt a remediation runs as, which is the number the machine's
    /// [`TaskState::Remediating`] holds and the directory the account is filed in.
    const SECOND: u32 = 2;

    /// The file the scratch repository's seed commit tracks — the only path a
    /// scripted session can change and the run can then commit, because
    /// [`crate::git::commit_all`] stages tracked paths and no one stages the rest.
    const SEED_FILE: &str = "seed.txt";

    /// What every scripted session prints, as two lines so a row per line is
    /// observable rather than assumed.
    const PRINTED: &str = "reading the seed\nwriting the fix\n";

    /// What a session that finished leaves in the report the prompt named.
    const DONE: &str = "KTASK_RESULT: DONE\nSummary: every step the task had is worked.\n";

    /// What a session that needs a decision from a human leaves instead.
    const NEEDS_INPUT: &str =
        "KTASK_RESULT: NEEDS_INPUT\nSummary: which of the two readings is meant?\n";

    /// What the scripted gate names the test it refuses over, in the cargo-shaped
    /// report it prints. A named failure is what a failure signature is built from,
    /// so a gate that refused without naming one would test less than this does.
    const REFUSED_TEST: &str = "the_fix";

    /// The report the gate prints when its test refuses, in cargo's own shape.
    const RED: &str = "running 1 tests\ntest the_fix ... FAILED\nfailures:\n    the_fix\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

    /// The same, once the fix is in the tree.
    const GREEN: &str = "running 1 tests\ntest the_fix ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

    /// The words every refusing completion gate is asked about, so no test has to
    /// repeat the sentence the bound wrote.
    const NOTHING_PROVED: &str = "verify: nothing is proved yet";

    /// The rows a run whose first attempt refused and whose second was repaired
    /// leaves for the task, oldest first.
    ///
    /// The completion set's own gate pairs are absent because
    /// [`crate::run_completion_set`] attributes them to no task — which is why the
    /// gate script's log, not the journal, is what says the set ran a second time.
    const REPAIRED: [&str; 22] = [
        "PreflightStarted",
        "PreflightPassed",
        "AttemptStarted",
        "PhaseEntered",
        "AgentOutput",
        "AgentOutput",
        "AttemptFinished",
        "GateStarted",
        "GateFinished",
        "AttemptStarted",
        "PhaseEntered",
        "AgentOutput",
        "AgentOutput",
        "AttemptFinished",
        "GateStarted",
        "GateFinished",
        "SelfHealingReport",
        "PhaseEntered",
        "VerifyPassed",
        "PublishStarted",
        "PublishVerified",
        "TaskDone",
    ];

    /// The gate script both of the project's gates call.
    ///
    /// One script answers the phase gate and the completion gate, and it answers
    /// them from what is in the tree rather than from a counter, because the thing
    /// under test is whether a second attempt *re-measured* the work: a script that
    /// answered from a plan file would pass for a run that replayed the first
    /// attempt's verdict instead of running the gate again.
    ///
    /// It prints cargo's own shape because [`crate::parse_cargo`] is what decides
    /// whether a refusing gate named a failure, and a signature built from nothing
    /// is a weaker breaker than one built from a name.
    fn gate_script(log: &Path, markers: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             m='{}'\n\
             echo \"$1\" >> '{}'\n\
             if [ \"$1\" = verify ]; then\n\
             if [ -f \"$m/refuse-first-verify\" ] && [ ! -f \"$m/verify-refused\" ]; then\n\
             touch \"$m/verify-refused\"\n\
             echo '{}' >&2\n\
             exit 1\n\
             fi\n\
             grep -qs '^fixed' seed.txt || {{ echo 'verify: the fix is not there' >&2; exit 1; }}\n\
             exit 0\n\
             fi\n\
             if [ -f \"$m/always-fails\" ]; then cat \"$m/red\"; exit 1; fi\n\
             if grep -qs '^fixed' seed.txt; then cat \"$m/green\"; exit 0; fi\n\
             cat \"$m/red\"\n\
             exit 1\n",
            markers.display(),
            log.display(),
            NOTHING_PROVED
        )
    }

    /// A registered project, the scenario its adapter replays, the script its two
    /// gates run, and the log and marker directory that script is driven by.
    ///
    /// The last three fields belong to the adapter every run here answers through
    /// ([`Scripted`]) rather than to the project: the reports it is to file, the
    /// invocations it is to remember, and how many sessions it is to refuse. They
    /// are shared rather than owned because the adapter is installed in the
    /// [`Runner`], and a test reads what it saw back out here.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
        scenario: PathBuf,
        markers: PathBuf,
        log: PathBuf,
        config_home: PathBuf,
        reports: Rc<RefCell<BTreeMap<u32, String>>>,
        asked: Rc<RefCell<Vec<Seen>>>,
        refusals: Rc<Cell<u32>>,
    }

    impl Fixture {
        /// A project on the configuration §7 describes: two attempts, one of them
        /// a remediation, and a breaker that has not been reached yet.
        fn new() -> Self {
            Self::built("")
        }

        /// As [`Fixture::new`], with the project allowing no remediation at all.
        ///
        /// `max_remediation_attempts = 0` is the configuration's own way of saying
        /// so (`docs/DESIGN.md` names the key), and a run against it stops at the
        /// first refusal. It is how a test looks at one refusal in isolation.
        fn single_attempt() -> Self {
            Self::built("max_remediation_attempts = 0\n")
        }

        /// As [`Fixture::new`], with `extra` appended to its settings document.
        fn built(extra: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            let scenario = repo.path().join("scenario.toml");
            let script = repo.path().join("gate.sh");
            let log = repo.path().join("gate.log");
            let markers = repo.path().join("markers");
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::create_dir_all(&markers).expect("a marker directory is creatable");
            fs::write(&script, gate_script(&log, &markers)).expect("a gate script is writable");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
                .expect("a gate script is made executable");
            let config_home = repo.path().join("config-home");
            fs::write(markers.join("red"), RED).expect("a gate report is writable");
            fs::write(markers.join("green"), GREEN).expect("a gate report is writable");
            fs::write(
                project_config_path(&project),
                settings(&scenario, &script, extra),
            )
            .expect("a project settings document is writable");
            Self {
                repo,
                project,
                scenario,
                markers,
                log,
                config_home,
                reports: Rc::default(),
                asked: Rc::default(),
                refusals: Rc::default(),
            }
        }

        /// The script the adapter replays, written before the run is opened:
        /// [`crate::provider::build`] reads the file once, so a scenario written
        /// after the run was built would be a scenario the run never saw.
        fn script(&self, document: &str) {
            fs::write(&self.scenario, document).expect("a scenario document is writable");
        }

        /// Leave `name` in the directory the gate script reads its instructions from.
        fn marker(&self, name: &str) {
            fs::write(self.markers.join(name), "").expect("a marker is writable");
        }

        /// Make the targeted gate refuse however many times it is asked.
        fn always_fails(&self) {
            self.marker("always-fails");
        }

        /// Make the completion gate refuse the first time it is asked and pass after.
        fn refuse_first_verify(&self) {
            self.marker("refuse-first-verify");
        }

        /// Hold `words` back as the report the session of attempt `attempt` leaves.
        ///
        /// Held rather than written where it belongs, because that place is the
        /// run's to open: [`Runner::begin_attempt`] files an attempt directory's
        /// `record.json` first and everything else after it, and [`read_evidence`]
        /// reports a directory below `attempts/<task>/` that holds no record as an
        /// interrupted write. Pre-writing a *later* attempt's report leaves exactly
        /// that shape behind while a remediation is still reading the prior evidence
        /// it is required to carry — a state no run reaches on its own, since
        /// nothing files a report before its own attempt is open. So the report
        /// travels with the adapter that answers for the session ([`Scripted`]) and
        /// is filed at the path the prompt named, which is what a session does.
        /// Staging nothing for an attempt leaves its report missing, which is a case
        /// of its own and stays testable.
        fn report(&self, attempt: u32, words: &str) {
            self.reports.borrow_mut().insert(attempt, words.to_owned());
        }

        /// Where one attempt's report would be, whether or not a session filed one.
        ///
        /// Read through [`report_path`] rather than spelled out here, so a test about
        /// a report that was never left cannot agree with itself if the layout moves.
        fn report_of(&self, attempt: u32) -> PathBuf {
            report_path(&self.project, TaskId::new(TASK), AttemptId::new(attempt))
        }

        /// The run this project is configured to have, with its sessions answered
        /// through [`Scripted`] — the adapter that files each session's report where
        /// that session was told to file it.
        fn run(&self) -> Runner {
            let mut run = Runner::new(self.project.clone())
                .expect("a registered, configured project opens a run");
            let answer = provider::build(&run.config).expect("the configured adapter is buildable");
            run.provider = Box::new(Scripted {
                inner: Inner {
                    adapter: answer,
                    sessions: Cell::new(0),
                },
                refusals: Rc::clone(&self.refusals),
                asked: Rc::clone(&self.asked),
                reports: Rc::clone(&self.reports),
            });
            run
        }

        /// Every invocation the installed adapter was handed, as it remembers them.
        fn asked(&self) -> Rc<RefCell<Vec<Seen>>> {
            Rc::clone(&self.asked)
        }

        /// Refuse this many sessions, as an adapter that could not be reached, before
        /// any is replayed.
        fn refuse_sessions(&self, times: u32) {
            self.refusals.set(times);
        }

        /// The environment a phase's prompt is read from: a configuration home of
        /// the fixture's own, so no test aims the prompt library at the machine
        /// running it.
        fn env(&self) -> impl Fn(&str) -> Option<String> {
            let home = self.config_home.clone();
            move |key: &str| (key == "XDG_CONFIG_HOME").then(|| home.display().to_string())
        }

        /// The gate names that actually ran, in the order they ran.
        fn ran(&self) -> Vec<String> {
            let text = fs::read_to_string(&self.log).unwrap_or_default();
            text.lines().map(str::to_owned).collect()
        }

        /// Every checkout the repository registers, the user's own included.
        fn checkouts(&self) -> Vec<git::Worktree> {
            git::list_worktrees(&self.project.root).expect("the repository answers what it holds")
        }

        /// The directory one attempt's own evidence is filed in.
        fn attempt_dir(&self, attempt: u32) -> PathBuf {
            evidence_dir(&self.project, TaskId::new(TASK), AttemptId::new(attempt))
        }

        /// The account §7 makes a recovery leave beside its own evidence.
        fn account(&self, attempt: u32) -> PathBuf {
            evidence_dir(&self.project, TaskId::new(TASK), AttemptId::new(attempt))
                .join("self-healing.md")
        }

        /// The tip the origin itself holds.
        fn origin_tip(&self) -> String {
            git::git(self.repo.origin(), &["rev-parse", "main"])
                .expect("the origin holds its branch")
        }
    }

    /// The settings a whole run is opened with: the scripted adapter and its
    /// scenario file, the phase gate and the completion gate as calls into one
    /// script, and `extra` appended.
    fn settings(scenario: &Path, script: &Path, extra: &str) -> String {
        format!(
            "provider = \"dummy\"\n\
             dummy_scenario_path = \"{}\"\n\
             min_free_disk_bytes = 1\n\
             targeted_test_command = [\"/bin/sh\", \"{}\", \"targeted\"]\n\
             verify_command = [\"/bin/sh\", \"{}\", \"verify\"]\n\
             {extra}",
            scenario.display(),
            script.display(),
            script.display()
        )
    }

    /// The queue's one task, worked by the project's default protocol.
    fn task() -> Task {
        let document = "\
## T096 Remediation in the runner

**Outcome:** a failed attempt is retried once, from a fresh session.
**Done-when:** a dummy scenario failing once then succeeding reaches Done.
**Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::/)'`
**Refs:** VISION.md section 7
";
        let parsed = parse_plan(document)
            .expect("a task block with the four mandatory sections is a parseable plan");
        let row = parsed
            .into_iter()
            .next()
            .expect("the fixture plan holds one row");
        assert_eq!(row.id, TaskId::new(TASK), "every fixture works task {TASK}");
        row
    }

    /// A scenario document's string, escaped the way TOML wants the breaks agent
    /// text is full of.
    fn toml_text(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    /// A scenario of one session per entry in `writes`, each leaving its own files
    /// in the checkout it runs in and printing `printed`.
    ///
    /// The adapter hands out one step per session it is asked to run, in order, so
    /// this is the whole script of a run: entry one is the first attempt's session
    /// and entry two the repair's.
    fn scripted(writes: &[Vec<(&str, &str)>], printed: &str) -> String {
        let mut document = String::new();
        for files in writes {
            writeln!(
                document,
                "[[steps]]\non_task = {TASK}\noutcome = \"success\"\nstdout = \"{}\"\n",
                toml_text(printed)
            )
            .expect("a String always has room for what is written into it");
            if !files.is_empty() {
                document.push_str("[steps.files]\n");
                for (path, contents) in files {
                    writeln!(document, "\"{path}\" = \"{}\"", toml_text(contents))
                        .expect("a String always has room for what is written into it");
                }
            }
            document.push('\n');
        }
        document
    }

    /// [`scripted`] with the ordinary two lines of session output.
    fn sessions(writes: &[Vec<(&str, &str)>]) -> String {
        scripted(writes, PRINTED)
    }

    /// The task's own rows, oldest first, read on a second connection because that
    /// is who asks this question in real life.
    fn rows(project: &Project) -> Vec<Event> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
    }

    /// The kinds the journal holds for the task, oldest first.
    fn kinds(project: &Project) -> Vec<&'static str> {
        rows(project)
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// Whether the journal holds a row of this kind for the task.
    fn holds(project: &Project, kind: &str) -> bool {
        kinds(project).contains(&kind)
    }

    /// The `detail` of the task's `TaskFailed` row, or `None` when it has none.
    fn failure(project: &Project) -> Option<(String, String)> {
        rows(project).iter().find_map(|row| match &row.kind {
            EventKind::TaskFailed { class, detail } => Some((format!("{class:?}"), detail.clone())),
            _ => None,
        })
    }

    /// The state the journal replays to, with an illegal row reported as the failure
    /// it is rather than as a state that was never reached.
    fn replayed(project: &Project) -> Option<TaskState> {
        let mut journal =
            Journal::open_for(project).expect("a registered project's journal is openable");
        journal
            .rebuild_state()
            .expect("every row this run wrote is one the state machine accepts");
        journal
            .get_state(TaskId::new(TASK))
            .expect("the projection is readable")
    }

    /// The attempt records the evidence directory holds, which is the mechanical
    /// answer to "how many attempts did this task actually have".
    fn attempts(project: &Project) -> Vec<AttemptId> {
        read_evidence(project, TaskId::new(TASK))
            .expect("an attempt's own record is readable from the moment it opened")
            .into_iter()
            .map(|record| record.id)
            .collect()
    }

    /// The commit the `TaskDone` row closed the task on.
    fn closed_on(project: &Project) -> String {
        for row in rows(project) {
            if let EventKind::TaskDone { commit } = row.kind {
                return commit;
            }
        }
        panic!("a finished task is closed on the commit it was published as");
    }

    /// Take the project's lock from a test, as an outsider would, and give it back.
    fn lock_is_free(project: &Project) -> bool {
        let Ok(held) = lock::acquire(&project.state_dir, Duration::ZERO) else {
            return false;
        };
        held.release().expect("a lock this test took is given back");
        true
    }

    /// Drive the queue's task to the end, with the prompt read from the fixture's
    /// own configuration home.
    fn finish(fixture: &Fixture, run: &mut Runner, work: &Task) -> TaskState {
        run.run_task_with(&fixture.env(), work, &Machine)
            .expect("nothing here gives a step a reason to refuse")
    }

    /// Drive the queue's task and hand back the refusal it stopped on.
    fn refusal(fixture: &Fixture, run: &mut Runner, work: &Task) -> Error {
        match run.run_task_with(&fixture.env(), work, &Machine) {
            Err(why) => why,
            Ok(state) => panic!("this fixture is built to stop the run, and it reached {state:?}"),
        }
    }

    /// Drive the queue's task to the stop that is not a refusal, and hand back the
    /// state its own journal says it stopped in.
    ///
    /// Kept apart from [`refusal`] because the two answer different questions, and a
    /// pause is the one stop `docs/CONTRACT.md` §1 insists is not a refusal: an
    /// assertion written against the wrong one of the two fails loudly rather than
    /// agreeing with a run that had turned a question into a fault.
    fn stopped(fixture: &Fixture, run: &mut Runner, work: &Task) -> TaskState {
        let state = run
            .run_task_with(&fixture.env(), work, &Machine)
            .expect("a question asked of a human is a pause, and a pause is not a refusal");
        assert!(
            state.is_paused(),
            "the run stopped somewhere on purpose: {state:?}"
        );
        state
    }

    /// One session an adapter was asked to run, as the adapter saw it.
    ///
    /// The three fields of an [`Invocation`] and nothing else: freshness of a
    /// session is not a field, and the absence of a session id in what the runner
    /// handed the second attempt over is what the test asks about.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        /// The prompt, in full — including whatever the runner appended to it.
        prompt: String,
        /// The model the configuration asked for.
        model: Option<String>,
        /// The directory the session was told to work in.
        working_dir: PathBuf,
    }

    /// The adapter [`Scripted`] hands its answers to, plus the count of how many
    /// sessions it has been asked to run.
    struct Inner {
        adapter: Box<dyn Provider>,
        sessions: Cell<u32>,
    }

    /// The adapter every session of every fixture run here goes through.
    ///
    /// Four things, each one a part of a real session a scripted scenario cannot
    /// reach on its own:
    ///
    /// - **It files the report.** An [`Invocation`] carries a prompt, and the prompt
    ///   names the path the report goes at; a session that answered writes there.
    ///   That is also the only way a *second* attempt's report can appear in the
    ///   directory the run opened a moment before it — see [`Fixture::report`] for
    ///   why no fixture may pre-write one. A session this adapter refused answers
    ///   nothing and files nothing, which is what an attempt that never had a
    ///   session looks like from the outside.
    /// - **It stamps each session with an id of its own.** The scripted adapter
    ///   reports none, and reading back the absence of a thing that was never there
    ///   proves nothing; §7's "no session id comes back" is only a finding once an
    ///   id existed to withhold.
    /// - **It remembers every [`Invocation`]**, because §7's two questions about the
    ///   session — what the second one was told, and whether the first one's
    ///   identity came back with it — are unaskable of the journal.
    /// - **It can refuse on cue**, as an adapter that could not be reached.
    struct Scripted {
        inner: Inner,
        /// Sessions to refuse before any is replayed, as a transient provider fault.
        refusals: Rc<Cell<u32>>,
        asked: Rc<RefCell<Vec<Seen>>>,
        reports: Rc<RefCell<BTreeMap<u32, String>>>,
    }

    impl Scripted {
        /// Leave this session's report at the path its prompt named, if the fixture
        /// staged words for the attempt that prompt is for.
        ///
        /// The directory is the one [`Runner::prepare_report`] made for this
        /// attempt, so a refusal to write here is not a scenario worth surviving: it
        /// says a run started a session in a directory it had not prepared, and a
        /// panic says that louder than a refusal the run would only go on to
        /// classify.
        fn file_report(&self, inv: &Invocation) {
            let (attempt, path) = told_by(&inv.prompt);
            let Some(words) = self.reports.borrow().get(&attempt).cloned() else {
                return;
            };
            fs::write(&path, words)
                .unwrap_or_else(|why| panic!("a session was told to report at {path:?}: {why}"));
        }
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            self.inner.adapter.name()
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.adapter.capabilities()
        }

        fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
            let session = self.inner.sessions.get() + 1;
            self.inner.sessions.set(session);
            self.asked.borrow_mut().push(Seen {
                prompt: inv.prompt.clone(),
                model: inv.model.clone(),
                working_dir: inv.working_dir.clone(),
            });
            if self.refusals.get() > 0 {
                self.refusals.set(self.refusals.get() - 1);
                return Err(Error::Provider {
                    provider: self.name().to_owned(),
                    detail: "the session could not be reached: connection reset by peer".to_owned(),
                });
            }
            let mut answer = self.inner.adapter.invoke(inv, bus)?;
            self.file_report(inv);
            answer.session_id = Some(format!("sess-attempt-{session}"));
            answer.usage = Some(Usage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                cached_tokens: None,
                cost_usd: None,
                source: UsageSource::Provider,
            });
            Ok(answer)
        }
    }

    /// Which attempt a prompt is for, and the path it says its report goes at.
    ///
    /// Read out of the prompt rather than recomputed, because what a session can act
    /// on is what it was *told*: a fixture that called [`report_path`] itself would
    /// agree with the header whatever the header said. The attempt is the name of the
    /// directory the report sits in, which is [`evidence_dir`]'s spelling of it.
    fn told_by(prompt: &str) -> (u32, PathBuf) {
        let named = prompt
            .split_once("Report: `")
            .expect("the prompt names the path its report goes at")
            .1;
        let path = PathBuf::from(
            named
                .split('`')
                .next()
                .expect("the path the prompt names is closed by a backtick"),
        );
        let directory = path
            .parent()
            .expect("a report is named below the directory it is filed in");
        let attempt = directory
            .file_name()
            .expect("the attempt's own directory name")
            .to_string_lossy()
            .parse::<u32>()
            .expect("that name is the attempt number");
        (attempt, path)
    }

    #[test]
    fn a_refused_attempt_that_the_repair_leaves_green_is_closed_with_two_attempts() {
        let fixture = Fixture::new();
        fixture.script(&sessions(&[
            vec![(SEED_FILE, "the work, which does not pass")],
            vec![(SEED_FILE, "fixed")],
        ]));
        fixture.report(FIRST, DONE);
        fixture.report(SECOND, DONE);
        let mut run = fixture.run();

        let state = finish(&fixture, &mut run, &task());

        assert_eq!(
            state,
            TaskState::Done,
            "the run's answer is the state its own journal folds to"
        );
        assert_eq!(
            kinds(&fixture.project),
            &REPAIRED[..],
            "one attempt's rows, then the repair's own attempt, its gate run again, the \
             account §7 asks for, and only then the ending"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "two attempts were opened, each with a record of its own"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "targeted", "verify"],
            "the refused gate ran again for the second attempt, and the completion set ran \
             once — against the repair"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(TaskState::Done),
            "every row the repair wrote is one the machine accepts, in the order it got them"
        );
        assert!(
            fixture.account(SECOND).is_file() && !fixture.account(FIRST).is_file(),
            "the recovery accounted for itself, in its own attempt's directory"
        );
        assert!(
            fixture.attempt_dir(FIRST).is_dir() && fixture.attempt_dir(SECOND).is_dir(),
            "each attempt files in a directory of its own, and the repair neither reused nor \
             erased the first one's"
        );
        assert_eq!(
            fixture.origin_tip(),
            closed_on(&fixture.project),
            "the task closed on the commit the remote was read back holding"
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "the shared checkout is removed once the work in it is published: {:?}",
            fixture.checkouts()
        );
        assert!(
            lock_is_free(&fixture.project),
            "and the lock is given back after both attempts"
        );
    }

    #[test]
    fn a_remediation_reruns_the_completion_set_from_scratch() {
        let fixture = Fixture::new();
        fixture.script(&sessions(&[
            vec![(SEED_FILE, "fixed")],
            vec![(
                SEED_FILE,
                "fixed again, which is what the second commit holds",
            )],
        ]));
        fixture.report(FIRST, DONE);
        fixture.report(SECOND, DONE);
        fixture.refuse_first_verify();
        let mut run = fixture.run();

        let state = finish(&fixture, &mut run, &task());

        assert_eq!(
            state,
            TaskState::Done,
            "a completion set that refused the first time passes against the repair"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "verify", "targeted", "verify"],
            "the whole set ran again for the second attempt: no verdict carried over, and \
             nothing was skipped because a gate had already been green"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "and the second of them is the repair"
        );
        assert_eq!(
            kinds(&fixture.project),
            REPAIRED[..9]
                .iter()
                .chain(["PhaseEntered", "VerifyFailed"].iter())
                .chain(REPAIRED[9..22].iter())
                .copied()
                .collect::<Vec<&'static str>>(),
            "the first attempt reached the ending and its refused verdict, and the second \
             worked the phase, accounted for itself and closed the task"
        );
        assert_eq!(
            fixture.origin_tip(),
            closed_on(&fixture.project),
            "the commit the remote holds is the one the second attempt's gates were run against"
        );
        assert!(lock_is_free(&fixture.project), "and the lock comes back");
    }

    #[test]
    fn a_repair_is_a_fresh_session_seeded_with_the_failure_and_no_session_id() {
        let fixture = Fixture::new();
        fixture.script(&sessions(&[
            vec![(SEED_FILE, "the work, which does not pass")],
            vec![(SEED_FILE, "fixed")],
        ]));
        fixture.report(FIRST, DONE);
        fixture.report(SECOND, DONE);
        let mut run = fixture.run();
        let seen = fixture.asked();

        let state = finish(&fixture, &mut run, &task());

        assert_eq!(state, TaskState::Done, "and the repair finished the task");
        let asked = seen.borrow().clone();
        assert_eq!(asked.len(), 2, "one session per attempt, no more");
        let first = &asked[0];
        let second = &asked[1];
        assert!(
            !first.prompt.contains("class:") && !first.prompt.contains("prior attempts:"),
            "the first session was seeded with the task alone — there was nothing to hand over"
        );
        assert!(
            second.prompt.contains("class: VerificationFailure"),
            "the repair is seeded with the classification, and its prompt was:\n{}",
            second.prompt
        );
        assert!(
            second.prompt.contains(REFUSED_TEST),
            "and with the gate output that named the failure"
        );
        assert!(
            second.prompt.contains("diff:") && second.prompt.contains(SEED_FILE),
            "and with the diff summary of what the failed attempt changed"
        );
        assert!(
            second.prompt.contains("prior attempts: 1"),
            "and with the prior attempt's outcome"
        );
        assert_eq!(
            second.working_dir, first.working_dir,
            "in the same checkout, which §7 requires be preserved"
        );
        assert_eq!(
            second.model, first.model,
            "on the same configured model, so the repair is not a different run of the task"
        );
        assert!(
            !second.prompt.contains("sess-attempt-1"),
            "and no session id came back: the repair is a new session, not a resumed one"
        );
        assert!(
            !first.prompt.contains("sess-attempt-2"),
            "and the ids are not in either prompt at all"
        );
    }

    #[test]
    fn the_second_identical_refusal_trips_the_breaker_and_ends_the_task() {
        let fixture = Fixture::built(
            "max_attempts = 5\nmax_remediation_attempts = 4\ncircuit_breaker_threshold = 2\n",
        );
        fixture.script(&sessions(&[
            vec![(SEED_FILE, "the first try")],
            vec![(SEED_FILE, "the second try, which differs")],
        ]));
        fixture.report(FIRST, DONE);
        fixture.report(SECOND, DONE);
        fixture.always_fails();
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::Gate { kind, .. } if kind == "targeted"),
            "the gate that refused is what the run hands back, and it was {why}"
        );
        let (class, detail) = failure(&fixture.project).expect("the trip ends the task");
        assert_eq!(
            class, "VerificationFailure",
            "the class the trip was counting"
        );
        assert!(
            detail.contains("circuit breaker tripped") && detail.contains("failed 2 times"),
            "and the record says the breaker spent the task on its second identical failure: {detail}"
        );
        assert_eq!(
            kinds(&fixture.project),
            REPAIRED[..16]
                .iter()
                .chain(["SelfHealingReport", "TaskFailed"].iter())
                .copied()
                .collect::<Vec<&'static str>>(),
            "two attempts, the repair's account of itself, and the row that ends it — and no \
             ending of the task"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "the breaker stopped the task at its bound, not before it"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "targeted"],
            "the gate ran once per attempt and no completion set was offered a candidate"
        );
        assert!(
            !holds(&fixture.project, "PublishStarted"),
            "nothing was offered to the remote"
        );
        assert_eq!(
            fixture.origin_tip(),
            fixture.repo.seed_sha(),
            "and the remote holds none of it"
        );
        assert_eq!(
            fixture.checkouts().len(),
            2,
            "the tree still holds both attempts' uncommitted work, so the checkout stays: {:?}",
            fixture.checkouts()
        );
        assert!(lock_is_free(&fixture.project), "while the lock comes back");
    }

    #[test]
    fn a_project_that_allows_no_remediation_stops_at_the_first_refusal() {
        let fixture = Fixture::single_attempt();
        fixture.script(&sessions(&[vec![(
            SEED_FILE,
            "the work, which does not pass",
        )]]));
        fixture.report(FIRST, DONE);
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::Gate { kind, .. } if kind == "targeted"),
            "the refusal is the gate's own, and it was {why}"
        );
        let (class, detail) = failure(&fixture.project).expect("a spent bound ends the task");
        assert_eq!(class, "VerificationFailure", "the class it refused on");
        assert_eq!(
            detail, "attempts 1 past the 0 bound",
            "the bound the configuration's zero ceiling spent"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST)],
            "and no second attempt was opened to spend a budget that does not exist"
        );
        assert_eq!(
            kinds(&fixture.project),
            REPAIRED[..9]
                .iter()
                .chain(["TaskFailed"].iter())
                .copied()
                .collect::<Vec<&'static str>>(),
            "one attempt's own rows and the row that ends the task — no second attempt, and \
             no account of a recovery that never happened"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted"],
            "one session, one gate, and no repair"
        );
        assert!(
            !holds(&fixture.project, "SelfHealingReport"),
            "nothing recovered, so nothing had an account to give"
        );
        assert!(lock_is_free(&fixture.project), "and the lock comes back");
    }

    #[test]
    fn a_repair_that_edits_the_rules_it_is_judged_by_ends_the_task() {
        let fixture = Fixture::new();
        fixture.script(&sessions(&[
            vec![(SEED_FILE, "the work, which does not pass")],
            vec![
                ("clippy.toml", "too-many-arguments-threshold = 100\n"),
                (SEED_FILE, "fixed"),
            ],
        ]));
        fixture.report(FIRST, DONE);
        fixture.report(SECOND, DONE);
        let mut run = fixture.run();

        let why = refusal(&fixture, &mut run, &task());

        assert!(
            matches!(&why, Error::Policy { paths, .. }
                if paths.contains(&PathBuf::from("clippy.toml"))),
            "the repair is refused by the rule it touched, and it was {why}"
        );
        let (class, detail) = failure(&fixture.project).expect("a policy edit ends the task");
        assert_eq!(
            class, "PolicyFailure",
            "§7's own class for a forbidden file"
        );
        assert!(
            detail.contains("clippy.toml"),
            "and the row names the file a human has to look at: {detail}"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted"],
            "the second attempt's gate never ran: the edit is refused before anything is \
             measured against it"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "the repair was opened, and ended by what it wrote"
        );
        assert!(
            fixture.account(SECOND).is_file(),
            "and it accounted for the repair it tried before it was stopped"
        );
        assert!(
            !holds(&fixture.project, "PublishStarted"),
            "nothing it wrote was offered to anyone"
        );
        assert_eq!(
            fixture.checkouts().len(),
            2,
            "the work it left stays where the next attempt is told to read it: {:?}",
            fixture.checkouts()
        );
        assert!(lock_is_free(&fixture.project), "and the lock comes back");
    }

    #[test]
    fn a_session_that_asks_for_input_is_never_looped_through() {
        let fixture = Fixture::new();
        fixture.script(&scripted(&[vec![]], NEEDS_INPUT));
        fixture.report(FIRST, NEEDS_INPUT);
        let mut run = fixture.run();

        let state = stopped(&fixture, &mut run, &task());

        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(FIRST),
                    phase: Phase::Implement,
                }),
            },
            "§3's eighth invariant makes the question a pause, and the pause holds where \
             the run stood: {state:?}"
        );
        assert_eq!(
            fixture.ran(),
            Vec::<String>::new(),
            "nothing was gated, because nothing was claimed to have been finished"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST)],
            "§7: needs_input never loops — no second session was launched to be asked again"
        );
        assert!(
            failure(&fixture.project).is_none(),
            "and the task is not marked failed for asking the question §3 makes a pause"
        );
        assert!(
            holds(&fixture.project, "Paused"),
            "the pause is a row and not only a return value: {:?}",
            kinds(&fixture.project)
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(state.clone()),
            "the journal says honestly where the run stopped, for whoever resumes it"
        );
        assert_eq!(
            apply(&state, &EventKind::Resumed).expect("a paused task accepts a resume"),
            TaskState::Running {
                attempt: AttemptId::new(FIRST),
                phase: Phase::Implement,
            },
            "and the state it stopped in is one a resume goes back to"
        );
        assert!(
            lock_is_free(&fixture.project),
            "and the lock comes back all the same"
        );
    }

    #[test]
    fn a_session_that_never_ran_is_classified_and_repaired_like_any_other() {
        let fixture = Fixture::new();
        fixture.script(&sessions(&[vec![(SEED_FILE, "fixed")]]));
        fixture.report(FIRST, DONE);
        fixture.report(SECOND, DONE);
        let mut run = fixture.run();
        fixture.refuse_sessions(1);

        let state = finish(&fixture, &mut run, &task());

        assert_eq!(
            state,
            TaskState::Done,
            "a provider that could not be reached once is repaired like any other failure"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "and the attempt that never had a session is recorded as having been made"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "verify"],
            "the gate of the attempt that never ran was never run"
        );
        assert!(
            !fixture.report_of(FIRST).exists(),
            "the attempt whose session was never reached filed no report, and none was \
             invented for it"
        );
        assert!(fixture.account(SECOND).is_file());
        assert!(lock_is_free(&fixture.project));
    }
}

#[cfg(test)]
mod pause {
    //! The three stops that are not failures: a limit, a gate, a question.
    //!
    //! `docs/CONTRACT.md` §1 gives a provider limit, a human gate and a decision
    //! request three exit codes — 3, 4 and 5 — and says of all three that they "are
    //! not failures and must never mark a task `failed`". VISION.md §6 names the
    //! states they park in (`waiting_limit`, `waiting_input`, `human_gate`), and §3's
    //! eighth invariant makes the last of the three the mechanism behind "nothing is
    //! done on an agent's say-so". So every test here asks the same question in a
    //! different costume: did the run *stop* without the journal saying the task was
    //! refused, and is where it stopped somewhere a later run can pick up from?
    //!
    //! The assertions are read out of records a third party keeps, the way
    //! [`mod@remediation`] reads its own: the journal replayed through
    //! [`crate::Journal::rebuild_state`] so an illegal row cannot pass, the gate
    //! script's log of which commands actually ran, the evidence directory's attempt
    //! count, and the origin's own tip for whether anything was published. What the
    //! run *returned* is asserted too — for a caller the pause *is* the return value —
    //! but never instead of the journal, because a screen reads the rows.
    //!
    //! The clock is the one thing here that cannot be a real one. A pause aimed at a
    //! reset the provider named is only testable against an instant the test decided,
    //! which is why [`Clock`] is a parameter of the run: [`Counted`] answers with one
    //! instant forever and keeps every plan it was asked to sit out, so a test can say
    //! both what time it was and whether this run slept. [`Machine`], the clock
    //! production uses, gets the two tests at the end: a fake clock proves nothing
    //! about the thread that really waits.

    use super::{Clock, Machine, Runner};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{
        AttemptId, Bus, Capabilities, Event, EventKind, Invocation, Journal, Outcome, PauseReason,
        Phase, Project, Provider, Result, Task, TaskId, TaskState, WaitPlan, apply, evidence_dir,
        git, lock, parse_plan, project_config_path, provider, read_evidence, report_path,
    };
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use time::OffsetDateTime;
    use time::macros::datetime;

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The queue position [`parse_plan`] gives the one-row plan below, and so the
    /// task every run here is driven for.
    const TASK: u32 = 1;

    /// The attempt a task that has never been attempted is opened at.
    const FIRST: u32 = 1;

    /// The attempt after a waited-out limit, which is a repair and so owes an account.
    const SECOND: u32 = 2;

    /// The file the scratch repository's seed commit tracks — the only path a
    /// scripted session can change and the run can then publish.
    const SEED_FILE: &str = "seed.txt";

    /// What every session that did its work prints, as two lines so a row per line is
    /// observable rather than assumed.
    const PRINTED: &str = "reading the seed\nwriting the fix\n";

    /// What a session that finished leaves in the report the prompt named.
    const DONE: &str = "KTASK_RESULT: DONE\nSummary: every step the task had is worked.\n";

    /// An ask whose body is short of every section a decision is made from, written
    /// both ways a run meets it: as the report an agent filed, and as the words a
    /// session printed instead of filing anything.
    const SHORT_ASK: &str =
        "KTASK_RESULT: NEEDS_INPUT\nSummary: which of the two readings is meant?\n";

    /// The complete ask: the same shortage, answered. The four parts
    /// [`crate::decision_request`] requires are all filled, which is what makes the
    /// row it asks for a [`EventKind::DecisionRaised`] rather than a plain pause.
    const FULL_ASK: &str = "KTASK_RESULT: NEEDS_INPUT\nSummary: which of the two readings \
                            is meant?\nQuestion: keep the pause's reason as text or as a \
                            table?\nOptions:\n- text\n- table\nTrade-offs: text costs no \
                            migration; a table costs one and buys a query.\nImpact: every \
                            later task that reads a pause reads this field.\n";

    /// The margin every fixture's settings ask a limit to be padded by, in seconds.
    const MARGIN_SECS: u64 = 30;

    /// The ceiling every fixture's settings ask a run to sit through, in seconds.
    const CEILING_SECS: u64 = 3600;

    /// A limit that names the instant it clears, as one provider prints it.
    const NAMED: &str = "ERROR: usage limit reached; resets at 2026-09-23T12:20:00Z";

    /// The same refusal with no instant in it, which is the other half of §7's table.
    const UNNAMED: &str = "ERROR: usage limit reached; try again later";

    /// A provider fault a fresh session can fix, which is the class a wait is not for.
    const DROPPED: &str = "ERROR: connection reset by peer while streaming";

    /// The gate report the scripted gate prints once the fix is in the tree.
    const GREEN: &str = "running 1 tests\ntest the_fix ... ok\n\ntest result: ok. 1 passed; 0 \
                        failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

    /// The instant every fixture's clock reads, so a reset the provider named is a
    /// distance from a point the test chose rather than from whenever it ran.
    fn now() -> OffsetDateTime {
        datetime!(2026-09-23 12:00:00 UTC)
    }

    /// The instant a run plans to wake at: the reset [`NAMED`] gives, twenty minutes
    /// after [`now`], plus the margin its settings ask for. Written out rather than
    /// added up from those two numbers, because a sum computed the way the code computes
    /// one would agree with the code whatever the code did.
    fn wake() -> OffsetDateTime {
        datetime!(2026-09-23 12:20:30 UTC)
    }

    /// The two rows a run writes once, before the first attempt it opens.
    const PREFLIGHT: [&str; 2] = ["PreflightStarted", "PreflightPassed"];

    /// The rows the attempt of a session that printed `words` leaves and no more:
    /// its entry, its phase, one output row per line it printed, its end.
    ///
    /// Nothing of a gate appears, because a session that was refused, or that asked a
    /// human something, claimed no finished work for a gate to judge. The output rows
    /// are counted from the session's own words rather than fixed, so the expectation
    /// says what the run is doing — journalling the output a session produced, line by
    /// line — instead of memorising one scenario's length.
    fn a_session_that_printed(words: &str) -> Vec<&'static str> {
        let mut rows = vec!["AttemptStarted", "PhaseEntered"];
        rows.extend((0..words.lines().count()).map(|_| "AgentOutput"));
        rows.push("AttemptFinished");
        rows
    }

    /// The gate script both of the project's gates call.
    ///
    /// It answers from what is in the tree rather than from a counter, so a gate can
    /// only come back green over work that is actually there: a run that paused, and
    /// was resumed into a session that wrote the fix, is proved by the second gate
    /// passing and not by a script that had already decided to pass.
    fn gate_script(log: &Path, markers: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             m='{}'\n\
             echo \"$1\" >> '{}'\n\
             if [ \"$1\" = verify ]; then\n\
             grep -qs '^fixed' seed.txt || {{ echo 'verify: nothing is proved yet' >&2; exit 1; \
             }}\n\
             exit 0\n\
             fi\n\
             if grep -qs '^fixed' seed.txt; then cat \"$m/green\"; exit 0; fi\n\
             echo 'targeted: nothing to gate yet' >&2\n\
             exit 1\n",
            markers.display(),
            log.display()
        )
    }

    /// A registered project, the scenario its adapter replays, and the script its two
    /// gates run.
    ///
    /// The last two fields belong to the adapter rather than to the project: the
    /// reports it is to file, and the prompts it was handed. They are shared rather
    /// than owned because the adapter is installed in the [`Runner`], and a test reads
    /// back what it saw from here.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
        scenario: PathBuf,
        log: PathBuf,
        config_home: PathBuf,
        reports: Rc<RefCell<BTreeMap<u32, String>>>,
        asked: Rc<RefCell<Vec<String>>>,
    }

    impl Fixture {
        /// A project whose settings give a limit a named margin and a named ceiling,
        /// so the deadline a pause carries is a number a test can spell out.
        fn new() -> Self {
            Self::built(&limits())
        }

        /// As [`Fixture::new`], on a project whose §7 bounds are already spent.
        ///
        /// `max_attempts = 0` makes the elapsed bound zero seconds, which is how a
        /// test asks whether a limit met by a run that has nothing left to spend still
        /// parks rather than launches an attempt past its own bound — and still parks
        /// with its deadline, because a spent budget is not a different limit.
        fn spent() -> Self {
            Self::built(&format!("{}\nmax_attempts = 0\n", limits()))
        }

        /// As [`Fixture::new`], with `extra` appended to its settings document.
        fn built(extra: &str) -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            let scenario = repo.path().join("scenario.toml");
            let script = repo.path().join("gate.sh");
            let log = repo.path().join("gate.log");
            let markers = repo.path().join("markers");
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            fs::create_dir_all(&markers).expect("a marker directory is creatable");
            fs::write(&script, gate_script(&log, &markers)).expect("a gate script is writable");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
                .expect("a gate script is made executable");
            let config_home = repo.path().join("config-home");
            fs::write(markers.join("green"), GREEN).expect("a gate report is writable");
            fs::write(
                project_config_path(&project),
                settings(&scenario, &script, extra),
            )
            .expect("a project settings document is writable");
            Self {
                repo,
                project,
                scenario,
                log,
                config_home,
                reports: Rc::default(),
                asked: Rc::default(),
            }
        }

        /// The script the adapter replays, written before the run is opened:
        /// [`crate::provider::build`] reads the file once.
        fn script(&self, document: &str) {
            fs::write(&self.scenario, document).expect("a scenario document is writable");
        }

        /// Hold `words` back as the report the session of attempt `attempt` leaves.
        ///
        /// Held rather than pre-written where it belongs, for the reason
        /// `mod remediation` spells out: the run owns that directory and treats a
        /// record-less attempt directory as an interrupted write. The adapter files it
        /// at the path the prompt named, which is what a session does.
        fn report(&self, attempt: u32, words: &str) {
            self.reports.borrow_mut().insert(attempt, words.to_owned());
        }

        /// Where one attempt's report would be, whether or not a session filed one.
        fn report_of(&self, attempt: u32) -> PathBuf {
            report_path(&self.project, TaskId::new(TASK), AttemptId::new(attempt))
        }

        /// The run this project is configured to have, with its sessions answered
        /// through [`Scripted`].
        fn run(&self) -> Runner {
            let mut run = Runner::new(self.project.clone())
                .expect("a registered, configured project opens a run");
            let answer = provider::build(&run.config).expect("the configured adapter is buildable");
            run.provider = Box::new(Scripted {
                inner: answer,
                asked: Rc::clone(&self.asked),
                reports: Rc::clone(&self.reports),
            });
            run
        }

        /// Every prompt the installed adapter was handed, in order.
        fn asked(&self) -> Vec<String> {
            self.asked.borrow().clone()
        }

        /// The environment a phase's prompt is read from: a configuration home of the
        /// fixture's own, so no test aims the prompt library at the machine running it.
        fn env(&self) -> impl Fn(&str) -> Option<String> {
            let home = self.config_home.clone();
            move |key: &str| (key == "XDG_CONFIG_HOME").then(|| home.display().to_string())
        }

        /// The gate names that actually ran, in the order they ran.
        fn ran(&self) -> Vec<String> {
            let text = fs::read_to_string(&self.log).unwrap_or_default();
            text.lines().map(str::to_owned).collect()
        }

        /// Every checkout the repository registers, the user's own included.
        fn checkouts(&self) -> Vec<git::Worktree> {
            git::list_worktrees(&self.project.root).expect("the repository answers what it holds")
        }

        /// The account §7 makes a recovery leave beside its own evidence.
        fn account(&self, attempt: u32) -> PathBuf {
            evidence_dir(&self.project, TaskId::new(TASK), AttemptId::new(attempt))
                .join("self-healing.md")
        }

        /// The tip the origin itself holds.
        fn origin_tip(&self) -> String {
            git::git(self.repo.origin(), &["rev-parse", "main"])
                .expect("the origin holds its branch")
        }
    }

    /// The two settings a limit is waited out under, as one §7 document's text.
    fn limits() -> String {
        format!("limit_wait_margin_secs = {MARGIN_SECS}\nlimit_max_wait_secs = {CEILING_SECS}\n")
    }

    /// The settings a whole run is opened with: the scripted adapter and its scenario
    /// file, the two gates as calls into one script, and `extra` appended.
    fn settings(scenario: &Path, script: &Path, extra: &str) -> String {
        format!(
            "provider = \"dummy\"\n\
             dummy_scenario_path = \"{}\"\n\
             min_free_disk_bytes = 1\n\
             targeted_test_command = [\"/bin/sh\", \"{}\", \"targeted\"]\n\
             verify_command = [\"/bin/sh\", \"{}\", \"verify\"]\n\
             {extra}",
            scenario.display(),
            script.display(),
            script.display()
        )
    }

    /// The queue's one task, worked by the project's default protocol.
    fn task() -> Task {
        parse_plan(&block("T097 Pause handling in the runner", ""))
            .expect("a task block with the four mandatory sections is a parseable plan")
            .into_iter()
            .next()
            .expect("the fixture plan holds one row")
    }

    /// The same queue entry written as a human gate: the same four sections, and the
    /// `**Gate:**` section that makes the task a decision a person owes.
    fn gate_task() -> Task {
        parse_plan(&block(
            "T097 Decide the schema",
            "**Gate:** approve the schema before any task is written against it.\n",
        ))
        .expect("a gate block with the four mandatory sections is a parseable plan")
        .into_iter()
        .next()
        .expect("the fixture plan holds one row")
    }

    /// A one-row plan document for a task titled `title`, with `extra` appended to the
    /// four mandatory sections.
    fn block(title: &str, extra: &str) -> String {
        format!(
            "## {title}\n\n\
             **Outcome:** limits, gates and input requests pause rather than fail.\n\
             **Done-when:** a dummy scenario reaches a resumable pause.\n\
             **Verify:** `cargo nextest run -p ktask-core -E 'test(/runner::/)'`\n\
             **Refs:** VISION.md section 6\n\
             {extra}"
        )
    }

    /// One session the scripted provider answers with, spelled as a scenario file
    /// spells it.
    struct Script {
        /// The outcome word the step declares.
        outcome: &'static str,
        /// Everything the session prints.
        printed: &'static str,
        /// The status it reports, or `None` for the one the word implies.
        exit_code: Option<i32>,
        /// How long it waits before answering, or `None` for no delay.
        delay_ms: Option<u64>,
        /// The files it leaves in the checkout it ran in.
        files: &'static [(&'static str, &'static str)],
    }

    impl Script {
        /// A session that did its work, printed `PRINTED` and left `files`.
        fn done(files: &'static [(&'static str, &'static str)]) -> Self {
            Self {
                outcome: "success",
                printed: PRINTED,
                exit_code: None,
                delay_ms: None,
                files,
            }
        }

        /// A session the provider refused for a limit, printing `words`.
        ///
        /// The status is declared rather than left to the outcome word, because a
        /// limit is read out of what a session printed and [`crate::classify`] only
        /// believes a session's prose when the session did not also claim it finished.
        fn limit(words: &'static str) -> Self {
            Self {
                outcome: "limit",
                printed: words,
                exit_code: Some(1),
                delay_ms: None,
                files: &[],
            }
        }

        /// A session that asked a human a question in its own output.
        fn spoken(words: &'static str) -> Self {
            Self {
                outcome: "needs_input",
                printed: words,
                exit_code: None,
                delay_ms: None,
                files: &[],
            }
        }

        /// A session the provider dropped mid-work, printing `words`.
        ///
        /// The status is declared for the reason [`Script::limit`] gives: the fault is
        /// read out of what the session printed, and [`crate::classify`] only believes
        /// a session's prose when the session did not also claim it finished.
        fn dropped(words: &'static str) -> Self {
            Self {
                outcome: "failure",
                printed: words,
                exit_code: Some(1),
                delay_ms: None,
                files: &[],
            }
        }

        /// Wait `millis` before answering, which is how a test gives a run an elapsed
        /// bound to notice.
        fn taking(self, millis: u64) -> Self {
            Self {
                delay_ms: Some(millis),
                ..self
            }
        }
    }

    /// The scenario document `scripts` replay, one step per session in order.
    fn scenario(scripts: &[Script]) -> String {
        let mut document = String::new();
        for step in scripts {
            writeln!(
                document,
                "[[steps]]\non_task = {TASK}\noutcome = \"{}\"\nstdout = \"{}\"",
                step.outcome,
                toml_text(step.printed)
            )
            .expect("a String always has room for what is written into it");
            if let Some(code) = step.exit_code {
                writeln!(document, "\nexit_code = {code}")
                    .expect("a String always has room for what is written into it");
            }
            if let Some(delay) = step.delay_ms {
                writeln!(document, "\ndelay_ms = {delay}")
                    .expect("a String always has room for what is written into it");
            }
            document.push('\n');
            if !step.files.is_empty() {
                document.push_str("[steps.files]\n");
                for (path, contents) in step.files {
                    writeln!(document, "\"{path}\" = \"{}\"", toml_text(contents))
                        .expect("a String always has room for what is written into it");
                }
            }
            document.push('\n');
        }
        document
    }

    /// A scenario document's string, escaped the way TOML wants the breaks agent text
    /// is full of.
    fn toml_text(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    /// The adapter every session of every fixture run here goes through.
    ///
    /// Two jobs the scripted provider cannot do on its own: it files the report the
    /// prompt said its session owed, because the path a session was *told* to write to
    /// is the only thing a report can be reached through; and it remembers every
    /// prompt, because §7's account of what the next session was told is unaskable of
    /// the journal. The answers themselves come from the configured dummy adapter
    /// unchanged — a pause is a fact about what the run did with an answer, and this
    /// adapter must not edit the answer.
    struct Scripted {
        inner: Box<dyn Provider>,
        asked: Rc<RefCell<Vec<String>>>,
        reports: Rc<RefCell<BTreeMap<u32, String>>>,
    }

    impl Scripted {
        /// Leave this session's report at the path its prompt named, if the fixture
        /// staged words for the attempt that prompt is for.
        fn file_report(&self, inv: &Invocation) {
            let (attempt, path) = told_by(&inv.prompt);
            let Some(words) = self.reports.borrow().get(&attempt).cloned() else {
                return;
            };
            fs::write(&path, words)
                .unwrap_or_else(|why| panic!("a session was told to report at {path:?}: {why}"));
        }
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
            self.asked.borrow_mut().push(inv.prompt.clone());
            let answer = self.inner.invoke(inv, bus)?;
            self.file_report(inv);
            Ok(answer)
        }
    }

    /// Which attempt a prompt is for, and the path it says its report goes at.
    ///
    /// Read out of the prompt rather than recomputed: what a session can act on is
    /// what it was told, and a fixture that called [`report_path`] itself would agree
    /// with the header whatever the header said.
    fn told_by(prompt: &str) -> (u32, PathBuf) {
        let named = prompt
            .split_once("Report: `")
            .expect("the prompt names the path its report goes at")
            .1;
        let path = PathBuf::from(
            named
                .split('`')
                .next()
                .expect("the path the prompt names is closed by a backtick"),
        );
        let directory = path
            .parent()
            .expect("a report is named below the directory it is filed in");
        let attempt = directory
            .file_name()
            .expect("the attempt's own directory name")
            .to_string_lossy()
            .parse::<u32>()
            .expect("that name is the attempt number");
        (attempt, path)
    }

    /// The clock a test decides the time with.
    ///
    /// One instant for every reading, so a deadline planned against it is exact rather
    /// than a window around whenever the test happened to run; and a recorded answer to
    /// *will this run sleep*, so a test can assert both that the wait was planned and
    /// that this run was, or was not, the one that sat it out.
    struct Counted {
        /// The instant every reading of [`Clock::now`] gives.
        instant: OffsetDateTime,
        /// Whether this clock sits a plan out, or answers that the run must return.
        will_wait: bool,
        /// Every plan this clock was asked about, in the order it was asked.
        plans: RefCell<Vec<WaitPlan>>,
    }

    impl Counted {
        /// A clock that reads `instant` forever and never sleeps.
        fn new(instant: OffsetDateTime) -> Self {
            Self {
                instant,
                will_wait: false,
                plans: RefCell::default(),
            }
        }

        /// As [`Counted::new`], for a run that does sit its waits out.
        fn waiting(instant: OffsetDateTime) -> Self {
            Self {
                will_wait: true,
                ..Self::new(instant)
            }
        }

        /// The plans this clock was asked to sit out, in order.
        fn plans(&self) -> Vec<WaitPlan> {
            self.plans.borrow().clone()
        }
    }

    impl Clock for Counted {
        fn now(&self) -> OffsetDateTime {
            self.instant
        }

        fn sit_out(&self, plan: WaitPlan) -> bool {
            self.plans.borrow_mut().push(plan);
            self.will_wait
        }
    }

    /// The task's own rows, oldest first, read on a second connection because that is
    /// who asks this question in real life.
    fn rows(project: &Project) -> Vec<Event> {
        Journal::open_for(project)
            .expect("a registered project's journal is openable")
            .events_for(TaskId::new(TASK))
            .expect("the rows this run wrote are readable")
    }

    /// The kinds the journal holds for the task, oldest first.
    fn kinds(project: &Project) -> Vec<&'static str> {
        rows(project)
            .iter()
            .map(|row| row.kind.discriminant())
            .collect()
    }

    /// Whether the journal holds a row of this kind for the task.
    fn holds(project: &Project, kind: &str) -> bool {
        kinds(project).contains(&kind)
    }

    /// The state the journal replays to, with an illegal row reported as the failure it
    /// is rather than as a state that was never reached.
    fn replayed(project: &Project) -> Option<TaskState> {
        let mut journal =
            Journal::open_for(project).expect("a registered project's journal is openable");
        journal
            .rebuild_state()
            .expect("every row this run wrote is one the state machine accepts");
        journal
            .get_state(TaskId::new(TASK))
            .expect("the projection is readable")
    }

    /// The attempt records the evidence directory holds, which is the mechanical answer
    /// to "how many attempts did this task actually have".
    fn attempts(project: &Project) -> Vec<AttemptId> {
        read_evidence(project, TaskId::new(TASK))
            .expect("an attempt's own record is readable from the moment it opened")
            .into_iter()
            .map(|record| record.id)
            .collect()
    }

    /// The commit the `TaskDone` row closed the task on.
    fn closed_on(project: &Project) -> String {
        for row in rows(project) {
            if let EventKind::TaskDone { commit } = row.kind {
                return commit;
            }
        }
        panic!("a finished task is closed on the commit it was published as");
    }

    /// Take the project's lock from a test, as an outsider would, and give it back.
    fn lock_is_free(project: &Project) -> bool {
        let Ok(held) = lock::acquire(&project.state_dir, Duration::ZERO) else {
            return false;
        };
        held.release().expect("a lock this test took is given back");
        true
    }

    /// Drive the queue's task against `clock` to a stop that is not a refusal, and hand
    /// back the state its own journal says it stopped in.
    fn parked(fixture: &Fixture, run: &mut Runner, work: &Task, clock: &dyn Clock) -> TaskState {
        let state = run
            .run_task_with(&fixture.env(), work, clock)
            .expect("a limit, a gate and a question are pauses, and no pause is a refusal");
        assert!(
            state.is_paused(),
            "§3's eighth invariant and CONTRACT.md §1 both make this a pause: {state:?}"
        );
        state
    }

    /// Drive the queue's task against `clock` to its end.
    fn finished(fixture: &Fixture, run: &mut Runner, work: &Task, clock: &dyn Clock) -> TaskState {
        run.run_task_with(&fixture.env(), work, clock)
            .expect("nothing here gives a waited-out run a reason to refuse")
    }

    /// The state a paused task goes back to when it is resumed.
    fn resumed(state: &TaskState) -> TaskState {
        apply(state, &EventKind::Resumed).expect("a pause is somewhere a run comes back from")
    }

    #[test]
    fn a_limit_that_named_its_reset_parks_with_that_deadline() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::limit(NAMED)]));
        let mut run = fixture.run();
        let before = fixture.origin_tip();
        let clock = Counted::new(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Limit {
                    until: Some(wake())
                },
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(FIRST),
                    phase: Phase::Implement,
                }),
            },
            "§7 waits a known reset out to the exact instant plus the margin, and the row \
             is written before anybody sleeps: {state:?}"
        );
        assert_eq!(
            clock.plans(),
            vec![WaitPlan::Deadline { at: wake() }],
            "the plan the provider's own words support is a deadline, not a backoff"
        );
        let mut wanted = PREFLIGHT.to_vec();
        wanted.extend(a_session_that_printed(NAMED));
        wanted.push("Paused");
        assert_eq!(
            kinds(&fixture.project),
            wanted,
            "the pause is the last row, and nothing after it was started"
        );
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "a limit is exit 3, not exit 1"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(state.clone()),
            "the journal replays to the pause the run returned"
        );
        assert_eq!(
            resumed(&state),
            TaskState::Running {
                attempt: AttemptId::new(FIRST),
                phase: Phase::Implement,
            },
            "and a resume goes back to the attempt that met the limit"
        );
        assert_eq!(
            fixture.origin_tip(),
            before,
            "nothing a paused attempt was doing was published"
        );
        assert!(
            lock_is_free(&fixture.project),
            "a pause gives the machine back"
        );
        assert_eq!(
            fixture.checkouts().len(),
            1,
            "a clean checkout is not kept, and the user's own is the one left: {:?}",
            fixture.checkouts()
        );
    }

    #[test]
    fn a_limit_that_named_no_reset_parks_with_a_bounded_backoff() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::limit(UNNAMED)]));
        let mut run = fixture.run();
        let clock = Counted::new(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        let TaskState::Paused {
            reason: PauseReason::Limit { until },
            ..
        } = &state
        else {
            panic!("a usage limit is the pause §7 names, and this run reached {state:?}");
        };
        assert_eq!(
            until, &None,
            "nothing here invents an instant the provider did not name"
        );
        assert_eq!(
            clock.plans(),
            vec![WaitPlan::Backoff {
                wait: time::Duration::seconds(i64::try_from(MARGIN_SECS).expect("a margin fits"))
            }],
            "an unknown reset is a bounded backoff of the configured pause"
        );
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "and it is still not a failure"
        );
    }

    #[test]
    fn a_limit_the_run_waited_out_costs_no_attempt_and_the_task_finishes() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[
            Script::limit(UNNAMED),
            Script::done(&[(SEED_FILE, "fixed")]),
        ]));
        fixture.report(SECOND, DONE);
        let mut run = fixture.run();
        let clock = Counted::waiting(now());

        let state = finished(&fixture, &mut run, &task(), &clock);

        assert_eq!(
            state,
            TaskState::Done,
            "a waited-out limit leaves the task where any other attempt would have"
        );
        assert_eq!(
            clock.plans().len(),
            1,
            "one wait was planned, and the run went on after it"
        );
        let journalled = kinds(&fixture.project);
        let slept = journalled
            .iter()
            .position(|kind| *kind == "Paused")
            .expect("a wait is journalled as a pause before anybody sleeps");
        let woke = journalled
            .iter()
            .position(|kind| *kind == "Resumed")
            .expect("and the wait is closed by a resume, which is what a restart reads");
        assert!(
            slept < woke && journalled.iter().filter(|kind| **kind == "Paused").count() == 1,
            "one pause, then one resume, then the work went on: {journalled:?}"
        );
        assert_eq!(
            fixture.asked().len(),
            2,
            "the wait bought one fresh session and no more"
        );
        assert!(
            fixture.asked()[1].contains("class: ProviderLimit"),
            "and that session was told what the last one was refused for"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "the wait itself is not an attempt: two, not three"
        );
        assert!(
            fixture.account(SECOND).is_file(),
            "the session after the wait is a repair, and accounts for itself"
        );
        assert_eq!(
            fixture.ran(),
            ["targeted", "verify"],
            "the resumed attempt was gated like any other"
        );
        assert_eq!(
            closed_on(&fixture.project),
            fixture.origin_tip(),
            "and finished work was published, pause and all"
        );
        assert!(lock_is_free(&fixture.project), "and the lock comes back");
    }

    /// §7 waits a limit out and asks everything else again, and the two answers are not
    /// interchangeable.
    ///
    /// The assertion is that the clock was handed *nothing*: a fault with no reset to
    /// wait for that is routed through the wait path still ends the task in the state an
    /// ordinary repair reaches, so only the wait it took, the rows it wrote and the class
    /// the next session was told give this one away. The clock is one that would have
    /// slept, so the emptiness is the run's refusal to ask rather than a clock that
    /// answered no.
    #[test]
    fn a_fault_that_is_not_a_limit_is_asked_again_without_a_wait() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[
            Script::dropped(DROPPED),
            Script::done(&[(SEED_FILE, "fixed")]),
        ]));
        fixture.report(SECOND, DONE);
        let mut run = fixture.run();
        let clock = Counted::waiting(now());

        let state = finished(&fixture, &mut run, &task(), &clock);

        assert_eq!(
            state,
            TaskState::Done,
            "a dropped session is repaired into a finished task like any other refusal: \
             {state:?}"
        );
        assert!(
            clock.plans().is_empty(),
            "a limit is the one refusal a run sleeps for, and a fault with no reset to wait \
             for was handed the clock as one: {:?}",
            clock.plans()
        );
        for kind in ["Paused", "Resumed"] {
            assert!(
                !holds(&fixture.project, kind),
                "nothing here paused, so the journal owes no {kind} row: {:?}",
                kinds(&fixture.project)
            );
        }
        assert_eq!(
            fixture.asked().len(),
            2,
            "asking again is one fresh session, with no wait in front of it"
        );
        assert!(
            fixture.asked()[1].contains("class: ProviderTransient"),
            "and the session after the fault was told what the last one was refused for"
        );
    }

    #[test]
    fn a_second_limit_in_one_run_parks_rather_than_waits_again() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::limit(UNNAMED), Script::limit(UNNAMED)]));
        let mut run = fixture.run();
        let clock = Counted::waiting(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        assert_eq!(
            clock.plans().len(),
            1,
            "the second limit is the provider contradicting the first wait, and a waiting \
             supervisor cannot settle that"
        );
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Limit { until: None },
                resume_to: Box::new(TaskState::Remediating {
                    attempt: AttemptId::new(SECOND),
                    phase: Phase::Implement,
                }),
            },
            "it parks at the attempt that met it: {state:?}"
        );
        let mut wanted = PREFLIGHT.to_vec();
        wanted.extend(a_session_that_printed(UNNAMED));
        wanted.extend(["Paused", "Resumed"]);
        wanted.extend(a_session_that_printed(UNNAMED));
        wanted.extend(["SelfHealingReport", "Paused"]);
        assert_eq!(
            kinds(&fixture.project),
            wanted,
            "one wait, one more attempt, and the account of it before the pause"
        );
        assert_eq!(
            attempts(&fixture.project),
            [AttemptId::new(FIRST), AttemptId::new(SECOND)],
            "both attempts are recorded"
        );
        assert!(fixture.account(SECOND).is_file());
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "and neither limit failed the task"
        );
        assert!(lock_is_free(&fixture.project));
    }

    #[test]
    fn a_limit_met_where_the_bounds_are_spent_parks_without_sleeping() {
        let fixture = Fixture::spent();
        fixture.script(&scenario(&[Script::limit(NAMED).taking(1500)]));
        let mut run = fixture.run();
        let clock = Counted::waiting(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        assert!(
            clock.plans().is_empty(),
            "a run out of its elapsed bound does not begin to sit a wait out: {:?}",
            clock.plans()
        );
        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Limit {
                    until: Some(wake())
                },
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(FIRST),
                    phase: Phase::Implement,
                }),
            },
            "and it is parked with the same deadline a run that could wait would have \
             written, because a spent budget is not a different limit: {state:?}"
        );
        assert_eq!(
            fixture.asked().len(),
            1,
            "no session was launched past the bound that stopped this run"
        );
        assert!(
            !holds(&fixture.project, "TaskFailed"),
            "a bound spent on a limit is still exit 3"
        );
    }

    #[test]
    fn a_report_that_asked_a_question_parks_the_attempt_that_wrote_it() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::done(&[])]));
        fixture.report(FIRST, FULL_ASK);
        let mut run = fixture.run();
        let clock = Counted::new(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(FIRST),
                    phase: Phase::Implement,
                }),
            },
            "§6's waiting_input is the state behind the eighth invariant: {state:?}"
        );
        let ask = rows(&fixture.project)
            .into_iter()
            .find_map(|row| match row.kind {
                EventKind::DecisionRaised { request } => Some(request),
                _ => None,
            });
        let ask = ask.expect("a complete ask is journalled as the decision it asks for");
        assert_eq!(
            ask.question, "keep the pause's reason as text or as a table?",
            "in the agent's own words"
        );
        assert_eq!(ask.options, ["text", "table"], "every choice it saw");
        assert!(!ask.tradeoffs.is_empty() && !ask.impact.is_empty());
        assert_eq!(
            kinds(&fixture.project).last(),
            Some(&"DecisionRaised"),
            "the ask is the row that parks it, and no second pause row is nested under it"
        );
        assert!(
            fixture.report_of(FIRST).is_file(),
            "the report itself stays filed as evidence"
        );
        assert_eq!(
            resumed(&state),
            TaskState::Running {
                attempt: AttemptId::new(FIRST),
                phase: Phase::Implement,
            }
        );
        assert!(lock_is_free(&fixture.project));
    }

    #[test]
    fn a_session_that_asked_in_its_output_alone_parks_without_an_ask() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::spoken(SHORT_ASK)]));
        let mut run = fixture.run();
        let clock = Counted::new(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        assert!(
            matches!(
                state,
                TaskState::Paused {
                    reason: PauseReason::Input,
                    ..
                }
            ),
            "a session that stopped at a decision has stopped at a decision, report or no \
             report: {state:?}"
        );
        assert!(
            !holds(&fixture.project, "DecisionRaised"),
            "nothing that could be read as a decision request was written, so none is \
             invented: {:?}",
            kinds(&fixture.project)
        );
        assert!(
            !fixture.report_of(FIRST).exists(),
            "and a pause does not file the report its session never wrote"
        );
        assert!(!holds(&fixture.project, "TaskFailed"));
        assert_eq!(fixture.ran(), Vec::<String>::new(), "nothing was gated");
    }

    #[test]
    fn an_ask_short_of_a_section_costs_the_ask_and_not_the_pause() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::done(&[])]));
        fixture.report(FIRST, SHORT_ASK);
        let mut run = fixture.run();
        let clock = Counted::new(now());

        let state = parked(&fixture, &mut run, &task(), &clock);

        assert!(
            matches!(
                state,
                TaskState::Paused {
                    reason: PauseReason::Input,
                    ..
                }
            ),
            "a report that claims NEEDS_INPUT stops the run whatever shape its body is in: \
             {state:?}"
        );
        assert!(
            !holds(&fixture.project, "DecisionRaised"),
            "and the shortage costs the structured ask, not the pause: {:?}",
            kinds(&fixture.project)
        );
        assert!(
            fixture.report_of(FIRST).is_file(),
            "the words it did write are kept where the prompt said to keep them"
        );
        assert!(
            !fixture.account(FIRST).is_file(),
            "a first attempt owes no §7 account, paused or otherwise"
        );
        assert!(!holds(&fixture.project, "TaskFailed"));
    }

    #[test]
    fn a_gate_task_parks_before_anything_is_started() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::done(&[])]));
        let mut run = fixture.run();
        let before = fixture.origin_tip();
        let clock = Counted::new(now());

        let state = parked(&fixture, &mut run, &gate_task(), &clock);

        assert_eq!(
            state,
            TaskState::Paused {
                reason: PauseReason::HumanGate,
                resume_to: Box::new(TaskState::Queued),
            },
            "§6 gives a gate to a person, so the task never left the queue: {state:?}"
        );
        assert_eq!(
            kinds(&fixture.project),
            ["Paused"],
            "one row, and no preflight, lock, checkout or session behind it"
        );
        assert_eq!(fixture.ran(), Vec::<String>::new(), "no gate command ran");
        assert!(fixture.asked().is_empty(), "no session was ever started");
        assert!(
            clock.plans().is_empty(),
            "a gate waits on a person, not on a clock"
        );
        assert_eq!(fixture.checkouts().len(), 1, "no checkout was cut for it");
        assert_eq!(fixture.origin_tip(), before, "and it produces no commit");
        assert!(
            lock_is_free(&fixture.project),
            "and it holds no lock while it waits"
        );
        assert_eq!(
            replayed(&fixture.project),
            Some(state.clone()),
            "the journal replays to the gate"
        );
        assert_eq!(
            apply(
                &state,
                &EventKind::GateAcknowledged {
                    by: "operator".to_owned(),
                    at: now(),
                }
            )
            .expect("a gate is acknowledged by a person"),
            TaskState::Acknowledged {
                by: "operator".to_owned(),
                at: now(),
            },
            "and `ack` is the only way out of it"
        );
    }

    #[test]
    fn a_gate_parked_twice_is_parked_once() {
        let fixture = Fixture::new();
        fixture.script(&scenario(&[Script::done(&[])]));
        let mut run = fixture.run();
        let clock = Counted::new(now());

        let first = parked(&fixture, &mut run, &gate_task(), &clock);
        let again = parked(&fixture, &mut run, &gate_task(), &clock);

        assert_eq!(first, again, "an operator pressing run twice sees one gate");
        assert_eq!(
            kinds(&fixture.project),
            ["Paused"],
            "and the journal holds one row, not a nested pause ADR-0026 would refuse"
        );
    }

    #[test]
    fn the_machines_clock_reads_the_instant_it_is_asked_at() {
        let before = OffsetDateTime::now_utc();
        let read = Machine.now();
        let after = OffsetDateTime::now_utc();

        assert!(
            read >= before && read <= after,
            "the production clock answers the instant it is asked at, which is the instant \
             a reset is measured against: {read:?} between {before:?} and {after:?}"
        );
    }

    #[test]
    fn the_machines_clock_sits_out_a_wait_that_has_not_passed() {
        let one_second = time::Duration::seconds(1);

        let started = Instant::now();
        assert!(
            Machine.sit_out(WaitPlan::Backoff { wait: one_second }),
            "a wait it is asked to sit out is a wait it reports having sat out"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "and it really slept: {:?}",
            started.elapsed()
        );

        let already = OffsetDateTime::now_utc().saturating_sub(one_second * 60);
        let passed = Instant::now();
        assert!(
            Machine.sit_out(WaitPlan::Deadline { at: already }),
            "a deadline already gone is still a wait that ended"
        );
        assert!(
            passed.elapsed() < Duration::from_millis(500),
            "without sleeping through anything that had already happened: {:?}",
            passed.elapsed()
        );
    }
}

#[cfg(test)]
mod outcome {
    //! What a command answers with, and the seven answers it may choose.
    //!
    //! `docs/DESIGN.md` *Other fixed types* fixes the variants and
    //! `docs/CONTRACT.md` §1 fixes what each of them *means*, including the rule
    //! this type exists for: three of the seven are pauses and "must never mark a
    //! task `failed`". Both properties belong to the type, so both are pinned
    //! here — which answers exist, what each carries, and that whoever reads this
    //! crate cannot mistake a pause for a run that failed.
    //!
    //! What is deliberately not asserted here is the number an answer turns into.
    //! §1's table belongs to the CLI that maps it, and a test in this crate that
    //! named 3 or 130 would put the numbers back into the layer whose whole point
    //! is that none are in it.

    use super::RunOutcome;
    use crate::TaskId;
    use std::collections::BTreeSet;
    use time::OffsetDateTime;
    use time::macros::datetime;

    /// What a wrong invocation is answered with: the argument the operator has to
    /// fix, so the refusal is actionable rather than merely negative.
    const USAGE: &str = "`--task` wants a number";

    /// The task a stopping answer names. Seven rather than one, so an answer that
    /// loses the id, or supplies somebody else's, cannot pass by coincidence.
    fn task() -> TaskId {
        TaskId::new(7)
    }

    /// The instant a provider said its ceiling lifts at, to the second.
    fn reset() -> OffsetDateTime {
        datetime!(2026-09-23 12:20:00 UTC)
    }

    /// Every outcome §1 documents, in the order its table lists them.
    fn documented() -> Vec<RunOutcome> {
        vec![
            RunOutcome::Drained,
            RunOutcome::TaskFailed { task: task() },
            RunOutcome::Usage {
                detail: USAGE.to_owned(),
            },
            RunOutcome::ProviderLimit {
                until: Some(reset()),
            },
            RunOutcome::HumanGate { task: task() },
            RunOutcome::NeedsInput { task: task() },
            RunOutcome::Interrupted,
        ]
    }

    /// What an answer means, in §1's words and with no number in sight.
    ///
    /// The match is total over the enum on purpose: an eighth variant arrives
    /// without its documented meaning only by this function being extended to say
    /// what it means, and two documented outcomes collapsed into one variant
    /// collapse two rows of the list into one word, which the first test refuses.
    fn meaning(answer: &RunOutcome) -> &'static str {
        match answer {
            RunOutcome::Drained => "the queue drained",
            RunOutcome::TaskFailed { .. } => "a task failed after its remediation budget",
            RunOutcome::Usage { .. } => "the command was used wrongly",
            RunOutcome::ProviderLimit { .. } => "the provider's ceiling paused the run",
            RunOutcome::HumanGate { .. } => "the run stopped at a human gate",
            RunOutcome::NeedsInput { .. } => "the run stopped on a question a person owes",
            RunOutcome::Interrupted => "the run was interrupted with its state durable",
        }
    }

    #[test]
    fn every_documented_outcome_is_its_own_answer() {
        let meanings: Vec<&str> = documented().iter().map(meaning).collect();
        let distinct: BTreeSet<&str> = meanings.iter().copied().collect();

        assert_eq!(
            meanings.len(),
            7,
            "§1 documents seven outcomes: {meanings:?}"
        );
        assert_eq!(
            distinct.len(),
            meanings.len(),
            "two documented outcomes share one variant, so one of them cannot be \
             answered: {meanings:?}"
        );
    }

    #[test]
    fn a_failure_names_the_task_a_retry_is_aimed_at() {
        let answer = RunOutcome::TaskFailed { task: task() };
        let RunOutcome::TaskFailed { task } = &answer else {
            panic!("the answer that stops a queue is the failed task: {answer:?}");
        };

        assert_eq!(
            *task,
            TaskId::new(7),
            "the id `retry --task` has to be given"
        );
        assert_ne!(
            answer,
            RunOutcome::TaskFailed {
                task: TaskId::new(8)
            },
            "the failure names the task that failed, not whichever one the run reached \
             last: {answer:?}"
        );
    }

    #[test]
    fn a_limit_carries_the_instant_the_provider_named() {
        let answer = RunOutcome::ProviderLimit {
            until: Some(reset()),
        };
        let RunOutcome::ProviderLimit { until } = &answer else {
            panic!("a ceiling is a pause, and a pause is an answer: {answer:?}");
        };

        assert_eq!(
            *until,
            Some(reset()),
            "the instant a resumed run wakes at is the one the provider gave, not one \
             re-derived after the wait"
        );
        assert_ne!(
            answer,
            RunOutcome::ProviderLimit { until: None },
            "a ceiling that named no reset is a different answer, because the wait was \
             the run's own back-off: {answer:?}"
        );
    }

    #[test]
    fn a_gate_and_a_question_each_name_the_task_they_stopped_at() {
        let gate = RunOutcome::HumanGate { task: task() };
        let question = RunOutcome::NeedsInput { task: task() };

        assert_eq!(
            gate,
            RunOutcome::HumanGate {
                task: TaskId::new(7)
            }
        );
        assert_eq!(
            question,
            RunOutcome::NeedsInput {
                task: TaskId::new(7)
            }
        );
        assert_ne!(
            gate, question,
            "a gate an `ack` passes is not a question a `resolve` answers: {gate:?} is \
             not {question:?}"
        );
    }

    #[test]
    fn a_usage_error_carries_the_thing_the_operator_has_to_fix() {
        let answer = RunOutcome::Usage {
            detail: USAGE.to_owned(),
        };
        let RunOutcome::Usage { detail } = &answer else {
            panic!("a wrong invocation is answered, not run: {answer:?}");
        };

        assert_eq!(
            *detail, USAGE,
            "the answer has to name the argument, section or missing project"
        );
        assert_ne!(
            answer,
            RunOutcome::Usage {
                detail: String::new()
            },
            "an empty detail is a refusal nobody can act on: {answer:?}"
        );
    }

    #[test]
    fn a_pause_is_never_the_answer_that_says_a_task_failed() {
        let pauses = [
            RunOutcome::ProviderLimit {
                until: Some(reset()),
            },
            RunOutcome::HumanGate { task: task() },
            RunOutcome::NeedsInput { task: task() },
        ];

        for pause in &pauses {
            assert!(
                !matches!(pause, RunOutcome::TaskFailed { .. }),
                "{} is a pause, and §1 says a pause must never mark a task failed: \
                 {pause:?}",
                meaning(pause)
            );
        }
    }

    #[test]
    fn only_a_drained_queue_is_the_answer_that_says_the_work_finished() {
        let stops: Vec<RunOutcome> = documented().into_iter().skip(1).collect();

        for stop in &stops {
            assert_ne!(
                stop,
                &RunOutcome::Drained,
                "{} stopped short of the end of the queue, so it cannot be the answer \
                 that says the queue drained: {stop:?}",
                meaning(stop)
            );
        }
    }
}

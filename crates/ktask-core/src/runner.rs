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
//! Its four jobs so far are [`Runner::prepare`], [`Runner::begin_attempt`],
//! [`Runner::run_phase`] and the round trip an attempt's report makes. The first takes a
//! queued task as far as the ground it stands on; the second is the
//! transition that spends a token, and three things about it are not free to
//! change:
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
//! What happens after a phase — its gate, the state its verdict moves, publication,
//! remediation — belongs to the tasks after this one, and this module starts no state
//! transition of its own.
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

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::statvfs;
use time::OffsetDateTime;

use crate::config;
use crate::git;
use crate::lock;
use crate::protocol;
use crate::provider;
use crate::{
    AttemptId, AttemptRecord, Bus, Capabilities, Config, Error, EventKind, FailureClass, Gate,
    GateKind, GateResult, Invocation, Journal, Phase, PhaseSpec, Profile, Project, Provider,
    Recorder, ReportClaim, ReportResult, Result, Stream, Subscription, Task, TaskId, profile_from,
    run_gate, write_evidence,
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
        let work = Some(task.id);
        self.recorder.record(
            work,
            EventKind::PhaseEntered {
                attempt,
                phase: spec.phase,
            },
        )?;
        let prompt = context::build_prompt_with(
            env,
            &self.project,
            task,
            attempt,
            queue::load(&self.project)?.len(),
        )?;
        self.prepare_report(task.id, attempt)?;
        let outcome = self.provider.invoke(
            &Invocation {
                prompt,
                model: self.config.model.clone(),
                working_dir: prep.worktree.clone(),
            },
            None,
        )?;
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
        let changed = git::changed_paths(&prep.worktree, &prep.base_sha)?;
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
        // A run out of budget is no verdict about the code. It is the same answer
        // [`crate::classify()`] gives a gate that never reached a verdict, so the
        // two halves of the supervisor cannot disagree about a timeout.
        class: if result.timed_out {
            FailureClass::EnvironmentFailure
        } else {
            FailureClass::VerificationFailure
        },
        detail,
    }
}

/// The one line a baseline run's evidence fits into: the command, how long it ran,
/// how it ended, and the last thing it said.
fn baseline_detail(gate: &Gate, result: &GateResult) -> String {
    let said = match last_words(result) {
        Some(line) => format!(", and its last line was `{line}`"),
        None => String::new(),
    };
    format!(
        "the baseline gate `{}` ran for {} ms and {}{said}",
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

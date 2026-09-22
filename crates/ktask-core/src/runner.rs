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
//! Its one job so far is [`Runner::begin_attempt`], the transition that spends a
//! token, and three things about it are not free to change:
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
//! What a run does once an attempt is open — phases, the agent session, the gates,
//! publication, remediation — belongs to the tasks after this one.
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
//! # What this module deliberately does not do
//!
//! - It starts no provider session. "Provider available" is answered from what a
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
use std::path::Path;
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
    GateKind, GateResult, Journal, Profile, Project, Provider, Recorder, Result, Subscription,
    Task, TaskId, profile_from, run_gate, write_evidence,
};

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

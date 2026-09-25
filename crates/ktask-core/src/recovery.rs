//! What a restart concludes about a run that stopped.
//!
//! VISION.md §6 makes recovery a feature rather than an edge case: on restart the
//! supervisor "inspects the live process table, the worktree, and the last persisted
//! transition, then either resumes the in-flight phase or marks the attempt
//! `interrupted`. It never guesses, and it never silently re-runs work that may
//! already have taken effect." [`reconcile`] is that inspection, and
//! [`crate::RecoveryDecision`] is what it leaves behind: one journaled row per
//! conclusion, with the evidence beside it.
//!
//! Four inputs are read, in that order. The events are folded into the state each task
//! is in — not read out of `task_state`, which ADR-0023 made disposable precisely so a
//! crash could be answered from the events. The kernel is asked about the `pid` the
//! journal recorded, with [`crate::lock`]'s one implementation of that question. Git is
//! asked which checkouts the repository registers, because the commit a publication was
//! making either exists or does not. And where the phase being decided is a
//! publication, the remote mainline is fetched and its tip compared with the candidate
//! the journal offered, because a checkout cannot tell a landed push from one that died
//! halfway: that is ADR-0097's fourth input, added to ADR-0096's three. No other phase
//! reaches it, so no other phase is asked about it — preflight, the gates and an agent's
//! session are all local work.
//!
//! What recovery never does is change the world: it commits nothing, pushes nothing,
//! creates and removes no worktree, and never takes the repository lock. Every one of
//! those side effects belongs to a command an operator started. The fetch above is the
//! one question asked beyond this machine, and it is asked to prove a publication
//! rather than to make one: it moves no ref anywhere. The whole of ADR-0096 is the rule
//! that turns these answers into one of §6's three verdicts, and the combinations it
//! refuses rather than guesses about.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{Error, Result};
use crate::event::EventKind;
use crate::git;
use crate::ids::{AttemptId, EventSeq, TaskId};
use crate::journal::Journal;
use crate::lock::{Life, life_of};
use crate::project::Project;
use crate::runner::worktree_name;
use crate::state::{Recovery, TaskState, apply, check_one_active};

/// What recovery decided about one task, and the evidence it decided it from.
///
/// The `detail` is journaled verbatim beside the verdict, so an operator can disagree
/// with one line of a recovery pass rather than with the whole of it. A decision whose
/// [`RecoveryDecision::task`] is `None` is about the queue rather than a task — the
/// one currently is the projection having to be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDecision {
    /// The task the verdict belongs to, or `None` for a verdict about the queue.
    pub task: Option<TaskId>,
    /// Which of VISION.md §6's three answers this is.
    pub decision: Recovery,
    /// The evidence, in the order recovery read it.
    pub detail: String,
}

/// Resolve a run that stopped to a known state.
///
/// Every task the journal knows about that has not finished is looked at: the last row
/// its own half of the journal holds, whether the process that row named is still
/// running, and whether the checkout named for it is there. Every conclusion is
/// appended to the journal as an [`EventKind::RecoveryDecision`] row once the whole
/// pass has been decided, and the projection is then rebuilt whenever it is not
/// already what the events say it is. A caller that reads [`Journal::all_states`]
/// after this returns is therefore reading the states these decisions move, never the
/// ones a crashed run left behind.
///
/// One verdict journals a second row. A publication the remote was read back holding
/// also gets the [`EventKind::PublishVerified`] row that reading is the proof of,
/// appended immediately before its decision row: the proof belongs to the journal, not
/// to the memory of the pass that read it, so the next restart reaches
/// [`TaskState::PublishedVerified`] from the events without asking the remote again.
///
/// Deciding is idempotent. A second pass over a task the first pass parked finds
/// [`TaskState::Paused`] and answers [`Recovery::AlreadyApplied`] without moving it, so
/// the projection never oscillates however often a run reconciles itself at start-up.
///
/// # Errors
///
/// [`Error::Corrupt`] when the journal contradicts itself: it does not replay, a state
/// holds an attempt no [`EventKind::AttemptStarted`] row ever claimed, a row's
/// recorded `pid` is 0 (refused before the machine is asked anything, because
/// signalling 0 reaches every process in this process group), or a finished task
/// was handed to the decider. [`Error::Policy`] when the
/// repository contradicts the journal — a checkout beside a task no transition was
/// journaled for, or a commit in a publication's checkout that nothing offered — and
/// when the verdicts would leave more than one task active, which is
/// [`crate::check_one_active`]'s invariant 1. [`Error::Git`] when `project`'s root
/// holds no repository, because the checkout is one of the inputs and guessing about
/// the others is not recovery; and when a publication's candidate has to be compared
/// with the remote mainline and the remote will not answer, because a push that cannot
/// be read back is a push whose fate only the remote knows. [`Error::Config`], with
/// [`Error::Io`] behind it, when the settings that name that remote and branch cannot
/// be read. Both questions are asked only of a pass that holds a publication: one that
/// stopped short of a published commit is decided without either.
///
/// A refusal writes nothing at all: every verdict and every resulting state is
/// computed before the first row is appended.
///
/// # Example
///
/// A journal that ends mid-attempt with a `pid` the machine no longer knows about
/// resolves to a parked task, and says so:
///
/// ```no_run
/// use ktask_core::{Journal, Project, discover, reconcile};
///
/// # fn main() -> ktask_core::Result<()> {
/// let project = discover(std::path::Path::new("."))?;
/// let mut journal = Journal::open_for(&project)?;
/// for decision in reconcile(&mut journal, &project)? {
///     println!("{:?}: {}", decision.task, decision.detail);
/// }
/// # Ok(())
/// # }
/// ```
pub fn reconcile(journal: &mut Journal, project: &Project) -> Result<Vec<RecoveryDecision>> {
    reconcile_with(journal, project, &life_of, &Mainline::for_project)
}

/// The clause every verdict carries when no attempt's process was asked about,
/// because the state the task is in names no attempt in flight.
const NO_PROCESS: &str = "no process was asked about, because no attempt of this task is in flight";

/// The clause every verdict carries when the remote mainline was not asked about,
/// because the phase being decided reaches no remote: preflight, the gates and an
/// agent's session are all local work, and only a publication's side effect is
/// visible anywhere but on this machine.
const NO_REMOTE: &str = "the remote was not asked, because nothing this phase does reaches it";

/// [`reconcile`], with the process table handed in.
///
/// Liveness is the one input this module cannot fabricate a test around: a fixture
/// cannot arrange to be a dead pid that a later run also cannot reuse. The rest of
/// recovery — the fold, the checkout, the decision table — is decided from what is
/// already on disk, so only this question is passed in. [`reconcile`] hands it
/// [`crate::lock`]'s answer; the tests hand it theirs.
fn reconcile_with(
    journal: &mut Journal,
    project: &Project,
    process: &dyn Fn(u32) -> Life,
    mainline: &dyn Fn(&Project) -> Result<Mainline>,
) -> Result<Vec<RecoveryDecision>> {
    let folded = journal.replayed_states()?;
    let projected = journal.all_states()?;
    let drift = drifted_rows(&projected, &folded);
    let mut facts = read_facts(journal)?;
    let checkouts = read_checkouts(&project.root)?;
    let mut states = folded.clone();
    let mut decided: Vec<(RecoveryDecision, Option<EventKind>)> = Vec::new();
    let ask_mainline = || {
        let names = mainline(project)?;
        let tip = names.fetched_tip(&project.root)?;
        Ok(Answered {
            remote: names.remote,
            branch: names.branch,
            tip,
        })
    };
    for task in known_tasks(journal, &folded)? {
        let checkout = checkouts
            .get(&worktree_name(task))
            .cloned()
            .unwrap_or(Checkout::Absent);
        let record = facts.remove(&task).unwrap_or_default();
        let state = folded.get(&task).cloned().unwrap_or(TaskState::Queued);
        if state.is_terminal() || (record.last.is_none() && matches!(checkout, Checkout::Absent)) {
            continue;
        }
        let verdict = decide(task, &state, &record, &checkout, process, &ask_mainline)?;
        let mut moved = state.clone();
        if let Some(proved) = &verdict.proved {
            moved = apply(&moved, proved)?;
        }
        moved = apply(
            &moved,
            &EventKind::RecoveryDecision {
                decision: verdict.decision,
                detail: verdict.detail.clone(),
            },
        )?;
        states.insert(task, moved);
        decided.push((
            RecoveryDecision {
                task: Some(task),
                decision: verdict.decision,
                detail: verdict.detail,
            },
            verdict.proved,
        ));
    }
    check_one_active(&states)?;
    let repaired = drift.into_iter().map(|detail| {
        (
            RecoveryDecision {
                task: None,
                decision: Recovery::AlreadyApplied,
                detail,
            },
            None,
        )
    });
    let verdicts = repaired.chain(decided).collect::<Vec<_>>();
    for (verdict, proved) in &verdicts {
        if let Some(row) = proved {
            journal.append(verdict.task, row)?;
        }
        journal.append(
            verdict.task,
            &EventKind::RecoveryDecision {
                decision: verdict.decision,
                detail: verdict.detail.clone(),
            },
        )?;
    }
    if states != projected {
        journal.rebuild_state()?;
    }
    Ok(verdicts.into_iter().map(|(verdict, _)| verdict).collect())
}

/// Which branch of which remote a publication had to move.
///
/// The two names come from the project's own settings rather than from a guess,
/// because §10's comparison is against *the* mainline: grading a candidate against
/// a branch nothing ever published to would read a publication as unmade and offer
/// the remote a second one.
#[derive(Debug)]
struct Mainline {
    /// The remote a publication pushes to and recovery fetches.
    remote: String,
    /// The branch a publication had to move.
    branch: String,
}

impl Mainline {
    /// The mainline the project's resolved settings name.
    ///
    /// This is asked only of a pass that holds a publication, because a run that
    /// stopped short of one is decided from the journal, the process table and the
    /// checkout: reading settings a verdict never uses would give a broken settings
    /// document a say about a recovery that has nothing to ask the remote.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] and [`Error::Io`] exactly as [`crate::config::load_for`]
    /// makes them, passed through unchanged. Settings that cannot be read are not a
    /// mainline to guess at.
    fn for_project(project: &Project) -> Result<Self> {
        let config = config::load_for(project)?;
        Ok(Self {
            remote: config.mainline_remote,
            branch: config.mainline_branch,
        })
    }

    /// The fully-qualified remote-tracking ref the fetched tip is read from.
    ///
    /// Qualified because `rev-parse main` answers about the *local* branch, which
    /// here is the commit the attempt started from rather than the one it was trying
    /// to publish. It always begins `refs/remotes/`, so neither name can reach git
    /// wearing an option's clothes.
    fn tracked_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote, self.branch)
    }

    /// Fetch the remote, and read back the tip it holds for this branch.
    ///
    /// The fetch is not decoration. `refs/remotes/<remote>/<branch>` is a cache of
    /// the last conversation with the remote, and [`crate::git::publish`] names it
    /// as the thing a fetch has to move before it can be believed — which is what
    /// makes a recovery that skips the fetch able to read a stale ref as a landed
    /// push, and a lie as one.
    ///
    /// # Errors
    ///
    /// [`Error::Git`] when the remote will not answer, or holds no such branch.
    /// Recovery refuses rather than guessing what a publication it cannot see the
    /// end of did.
    fn fetched_tip(&self, root: &Path) -> Result<String> {
        git::fetch(root, &self.remote)?;
        git::git(root, &["rev-parse", "--verify", &self.tracked_ref()])
    }
}

/// What the remote mainline answered when recovery asked it.
#[derive(Debug)]
struct Answered {
    /// The remote that was fetched.
    remote: String,
    /// The branch whose tip was read.
    branch: String,
    /// The tip that fetch brought back.
    tip: String,
}

impl Answered {
    /// The clause a verdict carries about what the remote said.
    fn clause(&self) -> String {
        format!(
            "`{}` was fetched and `{}` holds {}",
            self.remote, self.branch, self.tip
        )
    }
}

/// What recovery decided about one task, before any of it is journalled.
#[derive(Debug)]
struct Verdict {
    /// Which of §6's three answers this is.
    decision: Recovery,
    /// The evidence, in the order recovery read it.
    detail: String,
    /// A row the verdict read off the world that the machine needs before the task
    /// can stand where the verdict puts it. Only a publication whose push the remote
    /// was read back holding has one: [`EventKind::PublishVerified`] is the row §10's
    /// seventh step makes the whole difference between `Publishing` and
    /// [`TaskState::PublishedVerified`], and recovery has read that proof itself.
    /// Every other verdict's record is its decision row alone.
    proved: Option<EventKind>,
}

impl Verdict {
    /// A verdict whose decision row is the whole of its record.
    fn decided(decision: Recovery, detail: String) -> Self {
        Self {
            decision,
            detail,
            proved: None,
        }
    }
}

/// The verdict a phase answered with, when reading the world proved nothing the
/// machine does not already hold.
impl From<(Recovery, String)> for Verdict {
    fn from(answer: (Recovery, String)) -> Self {
        Self::decided(answer.0, answer.1)
    }
}

/// Where a task's checkout was found to be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Checkout {
    /// git registers no checkout for the task.
    Absent,
    /// git registers one whose directory is gone, which is why git calls it prunable.
    Missing {
        /// The directory git still registers.
        path: PathBuf,
    },
    /// git registers it, and `head` is the commit its `HEAD` points at.
    At {
        /// The directory git registers it at.
        path: PathBuf,
        /// The commit its `HEAD` points at, as git printed it.
        head: String,
    },
}

/// Everything the journal's rows say about one task that recovery asks about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Facts {
    /// The kind name of the last row the journal holds for the task.
    last: Option<String>,
    /// The process and base each [`EventKind::AttemptStarted`] row claimed.
    started: BTreeMap<AttemptId, Started>,
    /// The commit each [`EventKind::PublishStarted`] row offered.
    offered: BTreeMap<AttemptId, String>,
}

/// The two facts an attempt's row hands to recovery: who to ask about, and where the
/// work started from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Started {
    /// The process id the row named.
    pid: u32,
    /// The commit the attempt started from.
    base_sha: String,
}

/// The rows that say the projection drifted, when any do.
///
/// Only a row the projection holds is compared: a task the events know about and
/// the projection does not is not a lie but a gap, and `rebuild_state` fills gaps
/// as a matter of course. A row that names a state the events do not support is
/// the case ADR-0023 exists for — the projection was written by a run that
/// stopped before its own event landed — and it is reported before anything is
/// decided, because every verdict below is decided from the fold rather than from
/// the row that lied.
fn drifted_rows(
    projected: &BTreeMap<TaskId, TaskState>,
    folded: &BTreeMap<TaskId, TaskState>,
) -> Option<String> {
    let disagreed = projected
        .iter()
        .filter(|(task, state)| folded.get(*task) != Some(*state))
        .map(|(task, state)| {
            format!(
                "task {task} is {} in the projection and {} in the events",
                state.name(),
                folded.get(task).map_or("nothing at all", TaskState::name)
            )
        })
        .collect::<Vec<_>>();
    if disagreed.is_empty() {
        return None;
    }
    Some(format!(
        "{} of the projection's rows disagree with the journal ({}); the projection is rebuilt \
         from the events, which are the record, and nothing else is changed by this",
        disagreed.len(),
        disagreed.join("; ")
    ))
}

/// Every task the pass has to look at: the queue's own rows, and every task the
/// events name — the union, because a run that died mid-import can leave either
/// without the other.
fn known_tasks(
    journal: &Journal,
    folded: &BTreeMap<TaskId, TaskState>,
) -> Result<BTreeSet<TaskId>> {
    let mut ids = journal
        .tasks()?
        .iter()
        .map(|task| task.id)
        .collect::<BTreeSet<TaskId>>();
    ids.extend(folded.keys().copied());
    Ok(ids)
}

/// Everything the rows say about each task, gathered in one pass over the events.
///
/// The fold is not enough. A [`TaskState`] holds an attempt number and a phase,
/// and neither the `pid` the attempt ran under, nor the commit a publication had
/// offered, survive in it — while those two are exactly what the process table and
/// the checkout are asked about.
fn read_facts(journal: &Journal) -> Result<BTreeMap<TaskId, Facts>> {
    let mut facts: BTreeMap<TaskId, Facts> = BTreeMap::new();
    journal.for_each_event(EventSeq::new(0), &mut |event| {
        let Some(task) = event.task_id else {
            return Ok(());
        };
        let record = facts.entry(task).or_default();
        record.last = Some(event.kind.discriminant().to_owned());
        if let EventKind::AttemptStarted {
            attempt,
            pid,
            base_sha,
            ..
        } = &event.kind
        {
            record.started.insert(
                *attempt,
                Started {
                    pid: *pid,
                    base_sha: base_sha.clone(),
                },
            );
        }
        if let EventKind::PublishStarted {
            attempt,
            candidate_sha,
        } = &event.kind
        {
            record.offered.insert(*attempt, candidate_sha.clone());
        }
        Ok(())
    })?;
    Ok(facts)
}

/// The checkouts git registers for `root`, keyed by the name the runner gives a
/// task's worktree.
///
/// Only the managed directory is read: the main checkout belongs to whoever is
/// standing at the repository, and anything registered outside that directory is
/// not a task's checkout, whatever it is. One `git worktree list` answers for
/// every task rather than one call per task, and a checkout git calls prunable is
/// read as a directory that has gone rather than as no checkout at all — the
/// difference between a publication that was never started and one whose tree was
/// removed out from under it.
fn read_checkouts(root: &Path) -> Result<BTreeMap<String, Checkout>> {
    let managed = git::managed_dir(root)?;
    let mut found = BTreeMap::new();
    for entry in git::list_worktrees(root)? {
        let path = entry.path;
        if path.parent() != Some(managed.as_path()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let name = name.to_owned();
        let checkout = match entry.prunable {
            Some(_) => Checkout::Missing { path },
            None => Checkout::At {
                path,
                head: entry.head,
            },
        };
        found.insert(name, checkout);
    }
    Ok(found)
}

/// What one task's evidence means: which of VISION.md §6's three answers, and the
/// sentence that says why.
///
/// The table is ADR-0096, and its one organising idea is which half of the
/// pipeline a phase belongs to. A machine phase — preflight, the gates, the
/// publication — is a command the supervisor started and can start again, so a
/// dead process sends it forward whenever the tree it works on is still there. An
/// agent's phase is a session with half-written edits in a checkout, which no
/// supervisor can continue or roll back, so a dead process parks it. A live `pid`
/// always means adopt: recovery never starts a second owner of one checkout.
///
/// Two states need neither the process table nor the checkout. A queued entry that
/// began nothing has nothing to resume, and work the remote was read back holding
/// is proved whatever the machine looks like now.
fn decide(
    task: TaskId,
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
    process: &dyn Fn(u32) -> Life,
    mainline: &dyn Fn() -> Result<Answered>,
) -> Result<Verdict> {
    match state {
        TaskState::Queued => from_queue(task, state, facts, checkout),
        TaskState::Preflight => Ok(Verdict::decided(
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
                NO_REMOTE,
                "preflight is the supervisor's own checks and nothing an agent can corrupt, so they are simply run again",
            ),
        )),
        TaskState::Running { attempt, .. } | TaskState::Remediating { attempt, .. } => {
            Ok(agents(task, *attempt, state, facts, checkout, process)?.into())
        }
        TaskState::Verifying { attempt } => {
            Ok(gates(task, *attempt, state, facts, checkout, process)?.into())
        }
        TaskState::Publishing { attempt } => {
            publication(task, *attempt, state, facts, checkout, process, mainline)
        }
        TaskState::PublishedVerified { .. } => Ok(Verdict::decided(
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
                NO_REMOTE,
                "the remote was read back holding the commit, which no restart can unprove, so the publication is not made a second time",
            ),
        )),
        TaskState::Paused { .. } => Ok(Verdict::decided(
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
                NO_REMOTE,
                "the pause is durable and holds the state to return to; a restart lifts no gate and resumes no wait, so the task stays parked exactly where it was",
            ),
        )),
        finished @ (TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled) => Err(Error::Corrupt {
            detail: format!(
                "recovery was asked to decide about task {task}, which is already {}; a finished \
                 task has no phase left to resolve, so this is a caller's mistake rather than a \
                 state to reason about",
                finished.name()
            ),
            seq: None,
        }),
    }
}

/// A queued entry: either nothing happened, or the journal is not the whole story.
fn from_queue(
    task: TaskId,
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
) -> Result<Verdict> {
    match checkout {
        Checkout::Absent => Ok(Verdict::decided(
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
                NO_REMOTE,
                "nothing this task began is anywhere to be found, so there is no phase to resume \
                 and nothing that may already have taken effect",
            ),
        )),
        Checkout::Missing { path } | Checkout::At { path, .. } => Err(Error::Policy {
            detail: format!(
                "task {task} is Queued and git registers a checkout for it at `{}`; no transition \
                 was ever journaled for it, so recovery cannot say whether that checkout is this \
                 task's work or somebody else's, and it will not choose between them",
                path.display()
            ),
            paths: vec![path.clone()],
        }),
    }
}

/// An agent's own phase: `Running`, or the bounded `Remediating` above it.
///
/// The machine's answer about the recorded `pid` is the whole decision. A session
/// that still runs owns its checkout and its transcript, and a second one started
/// beside it would corrupt the work both are doing, so recovery adopts it and
/// changes nothing. A session that does not run cannot be continued mid-phase and
/// its half-written tree is not the supervisor's to guess about, so the attempt is
/// parked above the phase that stopped, which is where a `retry` starts from.
fn agents(
    task: TaskId,
    attempt: AttemptId,
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
    process: &dyn Fn(u32) -> Life,
) -> Result<(Recovery, String)> {
    let started = claim(facts, task, attempt, state)?;
    let (runs, asked) = liveness(started.pid, process);
    let (decision, because) = if runs {
        (
            Recovery::Resume,
            "the session that owns this checkout still runs, so it is adopted rather than \
             duplicated",
        )
    } else {
        (
            Recovery::MarkInterrupted,
            "no session is running the phase, and an agent's half-finished work can be neither \
             continued nor undone by guesswork",
        )
    };
    Ok((
        decision,
        evidence(state, facts, checkout, &asked, NO_REMOTE, because),
    ))
}

/// The gates phase: the supervisor's own commands, spent on one attempt's output.
///
/// Gates are commands, not sessions, so a dead process costs nothing but the run
/// of them — provided the tree they were spent on is still there to spend them on.
fn gates(
    task: TaskId,
    attempt: AttemptId,
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
    process: &dyn Fn(u32) -> Life,
) -> Result<(Recovery, String)> {
    let started = claim(facts, task, attempt, state)?;
    let (runs, asked) = liveness(started.pid, process);
    let (decision, because) = if runs {
        (
            Recovery::Resume,
            "the gates are still being spent on this checkout, so the run is waited for rather \
             than started again",
        )
    } else if matches!(checkout, Checkout::At { .. }) {
        (
            Recovery::Resume,
            "the gates are the supervisor's own commands and the checkout they check is still \
             there, so they are simply spent again",
        )
    } else {
        (
            Recovery::MarkInterrupted,
            "there is no checkout left to spend the gates on, so what was being verified no \
             longer exists",
        )
    };
    Ok((
        decision,
        evidence(state, facts, checkout, &asked, NO_REMOTE, because),
    ))
}

/// The publication: the one phase whose side effect outlives the process.
///
/// A commit is either there or it is not, and the checkout says which. The push is a
/// different question, and the checkout cannot answer it: a candidate sitting in
/// `HEAD` looks the same whether the push landed or died halfway. So VISION.md §10's
/// comparison is made against the only witness to a landed push — the remote is
/// fetched, and the tip that fetch brings back is compared with the candidate the
/// journal offered (ADR-0097).
///
/// Equal means the push landed: the task is moved to [`TaskState::PublishedVerified`]
/// by journalling the [`EventKind::PublishVerified`] row that reading is the proof of,
/// because that row is the whole of what separates the two states. Unequal means it
/// did not land, and the publication is retried — with the commit, if the checkout
/// already holds it, left alone rather than made a second time.
///
/// Recovery itself pushes nothing. What it reads is a `fetch` and a `rev-parse`, and
/// the decision to put a candidate in front of the remote again belongs to the
/// publication that gets restarted. A phase that offered no commit yet leaves nothing
/// to compare, so the remote is not asked about it at all: that verdict comes from the
/// checkout, and neither a settings document nor an unreachable remote gets a say over
/// work that never reached either.
fn publication(
    task: TaskId,
    attempt: AttemptId,
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
    process: &dyn Fn(u32) -> Life,
    mainline: &dyn Fn() -> Result<Answered>,
) -> Result<Verdict> {
    let started = claim(facts, task, attempt, state)?;
    let (runs, asked) = liveness(started.pid, process);
    if runs {
        return Ok(Verdict::decided(
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                NO_REMOTE,
                "the push is still in the air, so it is waited for rather than started again",
            ),
        ));
    }
    let offered = facts.offered.get(&attempt);
    let answered = match offered {
        Some(_) => Some(mainline()?),
        None => None,
    };
    let remote = answered
        .as_ref()
        .map_or_else(|| NO_REMOTE.to_owned(), Answered::clause);
    if let (Some(candidate), Some(answered)) = (offered, answered.as_ref())
        && answered.tip == *candidate
    {
        return Ok(Verdict {
            decision: Recovery::AlreadyApplied,
            detail: evidence(
                state,
                facts,
                checkout,
                &asked,
                &remote,
                "the remote holds the very commit the journal offered, so the push landed \
                 and offering it once more would publish the same work twice",
            ),
            proved: Some(EventKind::PublishVerified {
                commit: candidate.clone(),
                remote_sha: answered.tip.clone(),
            }),
        });
    }
    let Checkout::At { path, head } = checkout else {
        return Ok(Verdict::decided(
            Recovery::MarkInterrupted,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                &remote,
                "the checkout the commit was to be made in is gone, so there is nothing left \
                 for the publication to be made from",
            ),
        ));
    };
    if offered == Some(head) {
        return Ok(Verdict::decided(
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                &remote,
                "the checkout already holds the commit the journal offered and the remote does \
                 not, so that commit is not made a second time and only the push is retried",
            ),
        ));
    }
    if head == &started.base_sha {
        return Ok(Verdict::decided(
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                &remote,
                "the checkout still stands at the commit the attempt began from, so no commit was \
                 made and the publication can be run again",
            ),
        ));
    }
    Err(Error::Policy {
        detail: format!(
            "task {task}'s publication offered {} and its checkout stands at {head}, which is \
             neither that commit nor the attempt's base {}; recovery will not decide what a \
             commit no row offered is",
            offered.map_or_else(|| "nothing".to_owned(), Clone::clone),
            started.base_sha,
        ),
        paths: vec![path.clone()],
    })
}

/// The attempt's own row, or the damage report for a state naming an attempt the
/// journal never began.
///
/// A `pid` is recovery's only handle on a phase in progress, and an
/// [`EventKind::AttemptStarted`] row is the only place one is recorded: a state
/// naming an attempt with no such row claims a run that was never journaled, which
/// is damage rather than a process to go looking for. A recorded `pid` of 0 is
/// refused at the same point and for the same reason — it names no process, and
/// signalling 0 reaches every process in this process group, so the machine is
/// never asked.
fn claim<'a>(
    facts: &'a Facts,
    task: TaskId,
    attempt: AttemptId,
    state: &TaskState,
) -> Result<&'a Started> {
    let started = facts.started.get(&attempt).ok_or_else(|| Error::Corrupt {
        detail: format!(
            "task {task} is {state_name} for attempt {attempt}, and no AttemptStarted row ever \
             claimed that attempt; the process recovery would ask after is recorded nowhere",
            state_name = state.name(),
        ),
        seq: None,
    })?;
    if started.pid == 0 {
        return Err(Error::Corrupt {
            detail: format!(
                "task {task}'s attempt {attempt} recorded pid 0, which names no process; signalling \
                 pid 0 reaches every process in this process group, so it is refused before the \
                 machine is asked anything",
            ),
            seq: None,
        });
    }
    Ok(started)
}

/// Ask the machine about `pid`, and put the answer into the clause beside it.
///
/// The first half of the pair says whether a phase may be resumed on the strength
/// of the answer. Only [`Life::Running`] says so: a process recovery cannot
/// examine is a process it cannot adopt, and the reason the machine gave belongs
/// to the verdict rather than to a log line, because it is the fact an operator
/// would act on.
fn liveness(pid: u32, process: &dyn Fn(u32) -> Life) -> (bool, String) {
    match process(pid) {
        Life::Running {
            started: Some(ticks),
        } => (
            true,
            format!("pid {pid} runs, begun {ticks} ticks after boot"),
        ),
        Life::Running { started: None } => (true, format!("pid {pid} runs")),
        Life::Gone => (
            false,
            format!("pid {pid} is gone: the machine has no such process"),
        ),
        Life::OutOfReach { reason } => (
            false,
            format!("pid {pid} exists but cannot be examined: {reason}"),
        ),
    }
}

/// The sentence a verdict is journalled with: everything that was read, in the
/// order it was read, and the rule that turned it into the verdict.
fn evidence(
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
    asked: &str,
    remote: &str,
    because: &str,
) -> String {
    format!(
        "state {}{}; {}; {}; {}; the journal's last row is {}; {}",
        state.name(),
        whereof(state),
        asked,
        remote,
        describe(checkout),
        facts.last.as_deref().unwrap_or("no row at all"),
        because,
    )
}

/// The attempt and phase a state names, as the clause that reads it out.
fn whereof(state: &TaskState) -> String {
    match state {
        TaskState::Running { attempt, phase } | TaskState::Remediating { attempt, phase } => {
            format!(" (attempt {attempt}, phase {phase:?})")
        }
        TaskState::Verifying { attempt } | TaskState::Publishing { attempt } => {
            format!(" (attempt {attempt})")
        }
        _ => String::new(),
    }
}

/// The checkout clause: where the task's tree was found, and at what commit.
fn describe(checkout: &Checkout) -> String {
    match checkout {
        Checkout::Absent => "no checkout is registered for it".to_owned(),
        Checkout::Missing { path } => format!("its checkout `{}` is gone", path.display()),
        Checkout::At { path, head } => {
            format!("its checkout `{}` stands at {head}", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fmt::Write;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use crate::error::Error;
    use crate::event::EventKind;
    use crate::git;
    use crate::ids::{AttemptId, TaskId};
    use crate::journal::Journal;
    use crate::lock::Life;
    use crate::project::Project;
    use crate::project::project_config_path;
    use crate::recovery::{
        Answered, Checkout, Facts, Mainline, NO_REMOTE, RecoveryDecision, decide, reconcile,
        reconcile_with,
    };
    use crate::runner::worktree_name;
    use crate::state::{PauseReason, Phase, Recovery, TaskState};
    use crate::task::{Task, parse_plan};
    use crate::testing::{ScratchRepo, scratch_repo};

    /// The program a fixture runs to obtain a pid it then gives back to the kernel.
    const SLEEP: &str = "/bin/sleep";
    /// The id a fixture's project registers under.
    const PROJECT_ID: &str = "0123456789abcdef";
    /// The file the scratch repository's seed commit tracks, and the only one a
    /// worktree can therefore commit a change to.
    const SEED_FILE: &str = "seed.txt";

    /// A project of its own repository and its own state directory.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
    }

    impl Fixture {
        /// A scratch repository, and the state directory a registration would have made.
        fn new() -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let state_dir = repo.path().join("state").join(PROJECT_ID);
            fs::create_dir_all(&state_dir).expect("a state directory is creatable");
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir,
            };
            Self { repo, project }
        }

        /// The journal of this project's run.
        fn journal(&self) -> Journal {
            Journal::open_for(&self.project)
                .expect("a journal opens in a directory registration owns")
        }

        /// The commit a preflight would have handed this run.
        fn base(&self) -> String {
            self.repo.seed_sha().to_owned()
        }

        /// The checkout `task` would have been given, and which recovery reads.
        fn checkout_of(&self, task: TaskId) -> PathBuf {
            git::create_worktree(
                &self.project.root,
                &worktree_name(task),
                self.repo.seed_sha(),
            )
            .expect("a task checkout is creatable")
        }

        /// Commit a change inside `at`, and hand back what its `HEAD` holds after.
        fn commit_in(at: &Path, message: &str) -> String {
            fs::write(at.join(SEED_FILE), message).expect("a tracked file is writable");
            git::commit_all(at, message).expect("a change inside a checkout is committable");
            git::head_sha(at).expect("a checkout has a HEAD")
        }

        /// A commit on the mainline that no task's checkout stands on.
        fn elsewhere(&self) -> String {
            self.repo
                .commit("elsewhere.txt", "another task's work")
                .unwrap()
        }
    }

    /// A queue of `count` entries, in document order.
    fn queue(count: u32) -> Vec<Task> {
        let mut text = String::new();
        for id in 1..=count {
            write!(
                text,
                "## Task {id}\n\n**Outcome:** evidence {id} exists.\n\
                 **Done-when:** it is recorded.\n**Verify:** `true`\n\
                 **Refs:** VISION.md §6\n\n"
            )
            .expect("a String always has room for what is written into it");
        }
        parse_plan(&text).expect("the fixture plan parses")
    }

    /// Append one row about one task.
    fn row(journal: &mut Journal, task: TaskId, kind: &EventKind) {
        journal
            .append(Some(task), kind)
            .expect("a row appends to an open journal");
    }

    /// The rows that put `task` into `Preflight`.
    fn preflight_begins(journal: &mut Journal, task: TaskId) {
        row(journal, task, &EventKind::PreflightStarted);
    }

    /// The rows that put `task` into a work phase of `attempt`, run by `pid`.
    fn attempt_begins(journal: &mut Journal, task: TaskId, attempt: u32, pid: u32, base: &str) {
        preflight_begins(journal, task);
        row(
            journal,
            task,
            &EventKind::AttemptStarted {
                attempt: AttemptId::new(attempt),
                protocol: "direct".to_owned(),
                pid,
                base_sha: base.to_owned(),
            },
        );
        row(
            journal,
            task,
            &EventKind::PhaseEntered {
                attempt: AttemptId::new(attempt),
                phase: Phase::Implement,
            },
        );
    }

    /// The rows that put `task` into `Verifying` for `attempt`, run by `pid`.
    fn gates_begin(journal: &mut Journal, task: TaskId, attempt: u32, pid: u32, base: &str) {
        preflight_begins(journal, task);
        row(
            journal,
            task,
            &EventKind::AttemptStarted {
                attempt: AttemptId::new(attempt),
                protocol: "direct".to_owned(),
                pid,
                base_sha: base.to_owned(),
            },
        );
        row(
            journal,
            task,
            &EventKind::PhaseEntered {
                attempt: AttemptId::new(attempt),
                phase: Phase::Verify,
            },
        );
    }

    /// The rows that put `task` into a later attempt's work phase: the runner
    /// journals one [`EventKind::AttemptStarted`] for every attempt it starts,
    /// remediation included, and only the [`EventKind::PhaseEntered`] after it is
    /// what moves the state onto that attempt.
    fn attempt_continues(journal: &mut Journal, task: TaskId, attempt: u32, pid: u32, base: &str) {
        row(
            journal,
            task,
            &EventKind::AttemptStarted {
                attempt: AttemptId::new(attempt),
                protocol: "direct".to_owned(),
                pid,
                base_sha: base.to_owned(),
            },
        );
        row(
            journal,
            task,
            &EventKind::PhaseEntered {
                attempt: AttemptId::new(attempt),
                phase: Phase::Implement,
            },
        );
    }

    /// The rows that put `task` into `Publishing`, offering `candidate`.
    ///
    /// The passed gate comes first because the state machine only opens a
    /// publication with it, and the offer comes after it because the commit is
    /// what a publication is offering — a journal that skipped either would not be
    /// a run recovery could read, and recovery reads journals as they are.
    fn publication_begins(
        journal: &mut Journal,
        task: TaskId,
        pid: u32,
        base: &str,
        candidate: &str,
    ) {
        gates_begin(journal, task, 1, pid, base);
        row(
            journal,
            task,
            &EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
        );
        row(
            journal,
            task,
            &EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: candidate.to_owned(),
            },
        );
    }

    /// The rows that put `task` into `Publishing` with nothing offered yet.
    ///
    /// [`EventKind::VerifyPassed`] is what opens a publication, and the commit is
    /// offered after it, so a run that died between the two is in the phase with no
    /// candidate to compare a remote tip against.
    fn publication_opens(journal: &mut Journal, task: TaskId, pid: u32, base: &str) {
        gates_begin(journal, task, 1, pid, base);
        row(
            journal,
            task,
            &EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
        );
    }

    /// Decide with a process table that answers `answer` for every pid it is asked
    /// about, counting the times it was asked at all.
    ///
    /// The answer is a function rather than one [`Life`] because recovery asks once
    /// per task it looks at, and a fixture holding two tasks needs a machine that
    /// answers twice. The count is what lets a test insist that a `pid` recovery
    /// refuses to probe was never probed at all.
    fn decide_with(
        journal: &mut Journal,
        fixture: &Fixture,
        answer: fn(u32) -> Life,
        probes: &Cell<usize>,
    ) -> Result<Vec<RecoveryDecision>, Error> {
        reconcile_with(
            journal,
            &fixture.project,
            &|pid| {
                probes.set(probes.get() + 1);
                answer(pid)
            },
            &mainline(),
        )
    }

    /// The mainline a fixture's scratch repository publishes to, named rather than
    /// read out of the machine's settings: the fixture's remote is `origin` and its
    /// branch is `main`, and a test that resolved those names from the environment
    /// it happened to run in would be testing whoever set the environment.
    ///
    /// It is handed over as a resolver rather than as a [`Mainline`] because the
    /// question recovery asks is the one [`Mainline::for_project`] answers, and that
    /// question can fail: a stand-in for a fallible question is built in the shape of
    /// the question, so only the answer is pinned here.
    fn mainline() -> impl Fn(&Project) -> Result<Mainline, Error> {
        |_| {
            Ok(Mainline {
                remote: "origin".to_owned(),
                branch: "main".to_owned(),
            })
        }
    }

    /// A remote question a test never expects to be asked, because the phase it is
    /// deciding reaches no remote at all.
    fn never_asked() -> Result<Answered, Error> {
        Err(Error::Corrupt {
            detail: "recovery asked the remote about a phase that reaches no remote".to_owned(),
            seq: None,
        })
    }

    /// A process table for a fixture whose only pids are dead ones.
    fn dead(_pid: u32) -> Life {
        Life::Gone
    }

    /// A process table that finds a process it is not allowed to examine.
    fn unreachable_pid(_pid: u32) -> Life {
        Life::OutOfReach {
            reason: "operation not permitted".to_owned(),
        }
    }

    /// A process table that finds every `pid` running.
    fn alive(_pid: u32) -> Life {
        Life::Running { started: None }
    }

    /// A process table that answers for a pid which no longer exists.
    fn an_expired_pid() -> u32 {
        let mut child = Command::new(SLEEP)
            .arg("20")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fixture process starts");
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        pid
    }

    /// Every decision row the journal holds, in sequence order.
    fn decided(journal: &Journal) -> Vec<(Option<TaskId>, Recovery, String)> {
        journal
            .events()
            .expect("the journal is readable")
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::RecoveryDecision { decision, detail } => {
                    Some((event.task_id, decision, detail))
                }
                _ => None,
            })
            .collect()
    }

    /// The rows a task's projection is asked for, so a test can read a state back.
    fn projected(journal: &Journal, task: TaskId) -> Option<TaskState> {
        journal.get_state(task).expect("the projection is readable")
    }

    /// How many rows called `kind` the journal holds for `task`.
    ///
    /// The count of [`EventKind::PublishVerified`] is what tells a proved
    /// publication from one that merely was not pushed again, and the count of
    /// [`EventKind::PublishStarted`] is what tells a retried publication from a
    /// duplicated one. A verdict on its own cannot tell those apart.
    fn rows(journal: &Journal, task: TaskId, kind: &str) -> usize {
        journal
            .events()
            .expect("the journal is readable")
            .into_iter()
            .filter(|event| event.task_id == Some(task) && event.kind.discriminant() == kind)
            .count()
    }

    /// The commit the bare origin itself holds on `main`, read from the origin.
    ///
    /// Read off the remote rather than out of any remote-tracking ref, because the
    /// question under test is what the remote holds — and the ref in the fetching
    /// repository is one of the things being tested.
    fn remote_main(fixture: &Fixture) -> String {
        git::git(fixture.repo.origin(), &["rev-parse", "main"])
            .expect("the scratch origin was seeded with a main")
    }

    /// Write the cached remote-tracking ref by hand, without asking the remote.
    ///
    /// This is what a machine that has not spoken to `origin` since some earlier
    /// instant looks like: the cache is behind the truth, or — once a commit no push
    /// ever carried is written into it — ahead of it.
    fn remember_tip(fixture: &Fixture, sha: &str) {
        git::git(
            &fixture.project.root,
            &["update-ref", "refs/remotes/origin/main", sha],
        )
        .expect("a remote-tracking ref is writable");
    }

    /// Write the project's own settings document, naming the mainline it publishes
    /// to.
    ///
    /// The project's document outranks the machine's, which is what lets a test hold
    /// the two names recovery resolves without reading the environment the suite
    /// happens to run in.
    fn name_mainline(fixture: &Fixture, remote: &str, branch: &str) {
        fs::write(
            project_config_path(&fixture.project),
            format!("mainline_remote = \"{remote}\"\nmainline_branch = \"{branch}\"\n"),
        )
        .expect("a project's settings document is writable");
    }

    /// Point `origin` at a directory that is not there, so every question asked of
    /// the remote fails. A test that still gets an answer never asked for one.
    fn orphan_the_remote(fixture: &Fixture) {
        let gone = fixture.repo.path().join("gone.git");
        git::git(
            &fixture.project.root,
            &["remote", "set-url", "origin", &gone.display().to_string()],
        )
        .expect("a remote's url is rewritable");
    }

    /// Register a second remote called `name` at `url`, which is how a project comes
    /// to publish to a remote the settings name rather than the one `git clone` would
    /// have called `origin`.
    fn add_remote(fixture: &Fixture, name: &str, url: &Path) {
        git::git(
            &fixture.project.root,
            &["remote", "add", name, &url.display().to_string()],
        )
        .expect("a second remote is registerable");
    }

    #[test]
    fn a_projection_that_disagrees_with_the_journal_is_repaired_and_that_is_recorded() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);
        journal
            .put_state(task, &TaskState::Done)
            .expect("a projection row is writable");

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a drifted projection is repaired rather than argued about");

        assert_eq!(decisions.first().map(|found| found.task), Some(None));
        assert_eq!(decisions[0].decision, Recovery::AlreadyApplied);
        assert!(
            decisions[0].detail.contains("task 1"),
            "the repair names the task whose row disagreed: {}",
            decisions[0].detail
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Preflight),
            "the events, not the hand-written row, decide what the task is"
        );
    }

    #[test]
    fn a_projection_that_matches_the_journal_records_no_repair() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);
        journal
            .put_state(task, &TaskState::Preflight)
            .expect("a projection row is writable");

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a matching pass runs");

        assert_eq!(
            decisions.iter().filter(|one| one.task.is_none()).count(),
            0,
            "nothing drifted, so there is nothing to repair: {decisions:?}"
        );
    }

    #[test]
    fn a_queued_entry_that_began_nothing_is_applied_nothing() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        row(
            &mut journal,
            task,
            &EventKind::TaskQueued {
                title: "T".to_owned(),
            },
        );

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a queued pass runs");

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(decisions[0].task, Some(task));
        assert_eq!(decisions[0].decision, Recovery::AlreadyApplied);
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Queued),
            "a verdict that applies nothing leaves the entry queued"
        );
    }

    #[test]
    fn a_task_with_no_row_and_no_checkout_is_left_out_of_the_pass_entirely() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        journal.put_tasks(&queue(1)).expect("a queue is importable");

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("an untouched pass runs");

        assert!(decisions.is_empty(), "{decisions:?}");
        assert_eq!(decided(&journal), Vec::new(), "nothing was decided");
    }

    #[test]
    fn a_checkout_the_journal_never_started_is_refused_and_nothing_is_journalled() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        journal.put_tasks(&queue(1)).expect("a queue is importable");
        let at = fixture.checkout_of(task);

        let error = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect_err("a checkout with no journal behind it is not a phase to resume");

        assert!(matches!(error, Error::Policy { .. }), "{error}");
        assert!(
            error.to_string().contains(&at.display().to_string()),
            "the refusal names the checkout it could not explain: {error}"
        );
        assert_eq!(decided(&journal), Vec::new(), "a refusal decides nothing");
    }

    #[test]
    fn checks_that_stopped_mid_flight_resume() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a preflight pass runs");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert_eq!(projected(&journal, task), Some(TaskState::Preflight));
    }

    #[test]
    fn an_attempt_still_running_is_adopted_and_left_running() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        attempt_begins(&mut journal, task, 1, std::process::id(), &fixture.base());

        let decisions = reconcile(&mut journal, &fixture.project).expect("a live attempt is read");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }),
            "a run in progress is left in progress"
        );
    }

    #[test]
    fn an_attempt_whose_process_is_gone_is_parked_above_the_phase_it_stopped_in() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        attempt_begins(&mut journal, task, 1, 4_000_000, &fixture.base());

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a crashed pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::MarkInterrupted,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                }),
            }),
            "the phase that stopped is where a resume returns to"
        );
    }

    #[test]
    fn a_process_that_cannot_be_examined_cannot_be_adopted_and_says_why() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        attempt_begins(&mut journal, task, 1, 4_000_001, &fixture.base());
        let probes = Cell::new(0);

        let decisions = decide_with(&mut journal, &fixture, unreachable_pid, &probes)
            .expect("an unexaminable pid is an answer, not a crash");

        assert_eq!(
            decisions[0].decision,
            Recovery::MarkInterrupted,
            "{decisions:?}"
        );
        assert!(
            decisions[0].detail.contains("operation not permitted"),
            "the reason belongs to the verdict: {}",
            decisions[0].detail
        );
        assert_eq!(probes.get(), 1, "the pid was asked once");
    }

    #[test]
    fn the_real_process_table_answers_for_a_process_that_ran_and_finished() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let pid = an_expired_pid();
        attempt_begins(&mut journal, task, 1, pid, &fixture.base());

        let decisions = reconcile(&mut journal, &fixture.project).expect("a dead pid is readable");

        assert_eq!(
            decisions[0].decision,
            Recovery::MarkInterrupted,
            "a pid the machine gave back is not a run to adopt: {decisions:?}"
        );
    }

    #[test]
    fn a_remediation_whose_process_is_gone_is_parked_too() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        attempt_begins(&mut journal, task, 1, 4_000_002, &fixture.base());
        attempt_continues(&mut journal, task, 2, 4_000_002, &fixture.base());

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a remediation pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::MarkInterrupted,
            "{decisions:?}"
        );
        assert!(
            decisions[0].detail.contains("state Remediating (attempt 2"),
            "the verdict says which attempt it parked: {}",
            decisions[0].detail
        );
    }

    #[test]
    fn gates_that_lost_their_process_run_again_over_the_same_checkout() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        gates_begin(&mut journal, task, 1, 4_000_003, &fixture.base());
        fixture.checkout_of(task);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a verification pass runs");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Verifying {
                attempt: AttemptId::new(1)
            }),
            "gates the supervisor owns are simply run again"
        );
    }

    #[test]
    fn gates_with_no_checkout_to_verify_are_parked() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        gates_begin(&mut journal, task, 1, 4_000_004, &fixture.base());

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a bare verification pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::MarkInterrupted,
            "{decisions:?}"
        );
    }

    #[test]
    fn a_commit_the_journal_already_offered_is_not_made_a_second_time() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_005, &fixture.base(), &candidate);
        let held = remote_main(&fixture);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a mid-push pass runs");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert!(
            decisions[0].detail.contains(&candidate),
            "the verdict names the commit it refused to make again: {}",
            decisions[0].detail
        );
        assert!(
            decisions[0].detail.contains("not made a second time")
                && decisions[0].detail.contains("push is retried"),
            "the verdict says which half of the publication is repeated: {}",
            decisions[0].detail
        );
        assert_eq!(
            rows(&journal, task, "PublishVerified"),
            0,
            "a push the remote does not hold is not journalled as one that landed"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(1)
            }),
            "the task stays in the phase that has to finish"
        );
        assert_eq!(
            remote_main(&fixture),
            held,
            "recovery itself pushed nothing"
        );
    }

    #[test]
    fn a_publication_the_remote_was_read_back_holding_is_not_pushed_again() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_020, &fixture.base(), &candidate);
        git::publish(&at, "origin", "main", &candidate).expect("a candidate is pushable");

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a pass over a publication that landed runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::PublishedVerified {
                commit: candidate.clone()
            }),
            "the remote holding the candidate is the one fact Publishing waits for, so the task \
             stands where the push left it"
        );
        assert_eq!(
            rows(&journal, task, "PublishVerified"),
            1,
            "the proof recovery read off the remote is journalled, because a next restart has to \
             reach the same state without asking the remote again"
        );
        assert_eq!(
            rows(&journal, task, "PublishStarted"),
            1,
            "a publication the remote confirms is never started a second time"
        );
        assert_eq!(
            remote_main(&fixture),
            candidate,
            "the origin holds exactly the commit the journal offered"
        );
        assert!(
            decisions[0].detail.contains(&candidate),
            "the verdict names the commit the remote was found holding: {}",
            decisions[0].detail
        );
    }

    #[test]
    fn published_work_is_not_parked_even_after_its_checkout_is_removed() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_021, &fixture.base(), &candidate);
        git::publish(&at, "origin", "main", &candidate).expect("a candidate is pushable");
        fs::remove_dir_all(&at).expect("a checkout is removable by a test");

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a pass over published work with no checkout runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::PublishedVerified { commit: candidate }),
            "a vanished checkout cannot un-publish a commit the remote holds: the work is out, \
             and parking it would invite a second publication of it"
        );
    }

    #[test]
    fn a_publication_is_compared_with_the_mainline_the_projects_own_settings_name() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_026, &fixture.base(), &candidate);
        git::publish(&at, "origin", "trunk", &candidate)
            .expect("a candidate is pushable to a branch");
        add_remote(&fixture, "upstream", fixture.repo.origin());
        orphan_the_remote(&fixture);
        name_mainline(&fixture, "upstream", "trunk");

        let decisions = reconcile(&mut journal, &fixture.project)
            .expect("a pass that resolves the project's own names runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::PublishedVerified { commit: candidate }),
            "`upstream/trunk` is the mainline this project publishes to, and `origin` is a \
             remote that cannot answer: a pass that guessed the defaults instead of reading the \
             settings would fail on one name and retry a landed push on the other"
        );
    }

    #[test]
    fn a_landed_push_is_read_from_the_remote_and_not_from_the_cached_ref() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_022, &fixture.base(), &candidate);
        git::publish(&at, "origin", "main", &candidate).expect("a candidate is pushable");
        remember_tip(&fixture, &fixture.base());

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a pass over a publication with a stale ref runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::PublishedVerified { commit: candidate }),
            "the cached ref says the push never landed, and only the fetch knows it did"
        );
    }

    #[test]
    fn a_cached_ref_holding_what_the_remote_never_took_is_not_a_landed_push() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_023, &fixture.base(), &candidate);
        remember_tip(&fixture, &candidate);
        let held = remote_main(&fixture);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a pass over a publication with a lying ref runs");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert_eq!(
            rows(&journal, task, "PublishVerified"),
            0,
            "a ref that already holds the candidate proves nothing: the fetch is the witness"
        );
        assert_eq!(
            remote_main(&fixture),
            held,
            "recovery pushed nothing, and believing the ref is what would have let it"
        );
    }

    #[test]
    fn a_publication_the_remote_cannot_be_asked_about_refuses_to_decide() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(&mut journal, task, 4_000_024, &fixture.base(), &candidate);
        orphan_the_remote(&fixture);
        journal
            .put_state(
                task,
                &TaskState::Publishing {
                    attempt: AttemptId::new(1),
                },
            )
            .expect("a projection row is writable");

        let error = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect_err("a publication whose remote will not answer cannot be resolved");

        assert!(matches!(error, Error::Git { .. }), "{error}");
        assert_eq!(decided(&journal), Vec::new(), "a refusal decides nothing");
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(1)
            }),
            "a refusal writes nothing, so the task stays in the dangerous phase rather than \
             being moved by a guess"
        );
    }

    /// A push whose process is still alive is a fate still being made, not one to
    /// read back, so liveness is answered before the remote is — and the fixture's
    /// remote is left pointing at nothing to prove the pass really did not reach it.
    #[test]
    fn a_push_still_in_the_air_is_waited_for_without_asking_the_remote() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        publication_begins(
            &mut journal,
            task,
            std::process::id(),
            &fixture.base(),
            &candidate,
        );
        orphan_the_remote(&fixture);

        let decisions =
            reconcile(&mut journal, &fixture.project).expect("a live push is waited for");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert!(
            decisions[0].detail.contains(NO_REMOTE),
            "the verdict says the remote was not asked, and the unreachable remote the fixture \
             left behind is what proves that saying is true: {}",
            decisions[0].detail
        );
        assert_eq!(
            rows(&journal, task, "PublishVerified"),
            0,
            "a push still in progress is not journalled as one that landed"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Publishing {
                attempt: AttemptId::new(1)
            }),
            "the task is left in the phase its live push is still working through"
        );
    }

    #[test]
    fn a_publication_with_nothing_offered_yet_never_asks_the_remote() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        fixture.checkout_of(task);
        publication_opens(&mut journal, task, 4_000_025, &fixture.base());
        orphan_the_remote(&fixture);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a publication that offered no commit needs no answer from the remote");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
        assert!(
            decisions[0].detail.contains(NO_REMOTE),
            "the verdict says the remote was not asked, and the unreachable remote in the \
             fixture is what proves it said true: {}",
            decisions[0].detail
        );
    }

    #[test]
    fn a_publication_still_at_its_base_commit_resumes() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let candidate = fixture.elsewhere();
        fixture.checkout_of(task);
        publication_begins(&mut journal, task, 4_000_006, &fixture.base(), &candidate);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a pre-commit pass runs");

        assert_eq!(decisions[0].decision, Recovery::Resume, "{decisions:?}");
    }

    #[test]
    fn a_commit_the_journal_never_offered_is_refused_not_explained_away() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let candidate = fixture.elsewhere();
        let at = fixture.checkout_of(task);
        let found = Fixture::commit_in(&at, "a commit nobody offered");
        publication_begins(&mut journal, task, 4_000_007, &fixture.base(), &candidate);

        let error = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect_err("a commit no row offered is not work to adopt");

        assert!(matches!(error, Error::Policy { .. }), "{error}");
        assert!(
            error.to_string().contains(&found),
            "the refusal names the commit it cannot account for: {error}"
        );
        assert_eq!(decided(&journal), Vec::new(), "a refusal decides nothing");
    }

    #[test]
    fn a_publication_whose_checkout_has_vanished_is_parked() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let at = fixture.checkout_of(task);
        let candidate = Fixture::commit_in(&at, "the candidate");
        fs::remove_dir_all(&at).expect("a checkout is removable by a test");
        publication_begins(&mut journal, task, 4_000_008, &fixture.base(), &candidate);

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a vanished pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::MarkInterrupted,
            "{decisions:?}"
        );
        assert!(
            decisions[0].detail.contains("gone"),
            "the verdict says what happened to the checkout: {}",
            decisions[0].detail
        );
    }

    #[test]
    fn published_work_waits_for_its_own_done_row_and_nothing_else() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let candidate = fixture.elsewhere();
        publication_begins(&mut journal, task, 4_000_009, &fixture.base(), &candidate);
        row(
            &mut journal,
            task,
            &EventKind::PublishVerified {
                commit: candidate.clone(),
                remote_sha: candidate.clone(),
            },
        );

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a published pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::PublishedVerified { commit: candidate }),
            "proved work stays proved"
        );
    }

    #[test]
    fn a_pause_at_a_gate_is_left_parked_exactly_as_it_was() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);
        row(
            &mut journal,
            task,
            &EventKind::Paused {
                reason: PauseReason::HumanGate,
            },
        );

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a parked pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Paused {
                reason: PauseReason::HumanGate,
                resume_to: Box::new(TaskState::Preflight),
            }),
            "a gate is lifted by `ack`, not by a restart"
        );
    }

    #[test]
    fn a_second_pass_over_a_parked_attempt_records_the_same_answer_and_moves_nothing() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        attempt_begins(&mut journal, task, 1, 4_000_010, &fixture.base());
        reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("the first pass runs");
        let parked = projected(&journal, task);

        let second = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("the second pass runs");

        assert_eq!(second[0].decision, Recovery::AlreadyApplied, "{second:?}");
        assert_eq!(
            projected(&journal, task),
            parked,
            "reconciling twice does not walk a task off the place it was parked in"
        );
    }

    #[test]
    fn an_attempt_the_journal_never_recorded_is_damage_not_a_process_to_ask() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);
        row(
            &mut journal,
            task,
            &EventKind::PhaseEntered {
                attempt: AttemptId::new(7),
                phase: Phase::Implement,
            },
        );

        let error = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect_err("a state naming an attempt no row began is damage");

        assert!(matches!(error, Error::Corrupt { .. }), "{error}");
        assert_eq!(decided(&journal), Vec::new(), "a refusal decides nothing");
    }

    #[test]
    fn a_recorded_pid_of_zero_is_refused_before_the_machine_is_asked() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        attempt_begins(&mut journal, task, 1, 0, &fixture.base());
        let probes = Cell::new(0);

        let error = decide_with(&mut journal, &fixture, dead, &probes)
            .expect_err("pid 0 is no process, and signalling it signals this group");

        assert!(matches!(error, Error::Corrupt { .. }), "{error}");
        assert_eq!(probes.get(), 0, "pid 0 is never probed");
    }

    #[test]
    fn two_live_attempts_are_refused_rather_than_resumed_beside_each_other() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        attempt_begins(&mut journal, TaskId::new(1), 1, 4_000_011, &fixture.base());
        attempt_begins(&mut journal, TaskId::new(2), 1, 4_000_012, &fixture.base());
        let probes = Cell::new(0);

        let error = decide_with(&mut journal, &fixture, alive, &probes)
            .expect_err("two resumable attempts break invariant 1");

        assert!(matches!(error, Error::Policy { .. }), "{error}");
        assert_eq!(decided(&journal), Vec::new(), "a refusal decides nothing");
    }

    #[test]
    fn a_journal_that_does_not_replay_is_reported_with_the_row_that_broke_it() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);
        row(
            &mut journal,
            task,
            &EventKind::TaskDone {
                commit: fixture.base(),
            },
        );

        let error = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect_err("a refused row is damage");

        match error {
            Error::Corrupt { detail, seq } => {
                assert!(seq.is_some(), "the reader is told which row: {detail}");
            }
            other => panic!("a journal that will not fold is damage, not {other}"),
        }
    }

    #[test]
    fn a_project_whose_root_holds_no_repository_is_refused() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        preflight_begins(&mut journal, TaskId::new(1));
        let outside = fixture.repo.path().join("not-a-repository");
        fs::create_dir(&outside).expect("a plain directory is creatable");
        let project = Project {
            root: outside.clone(),
            id: PROJECT_ID.to_owned(),
            state_dir: fixture.project.state_dir.clone(),
        };

        let error = reconcile_with(&mut journal, &project, &dead, &mainline())
            .expect_err("a checkout that cannot be read is not a question to skip");

        assert!(matches!(error, Error::Git { .. }), "{error}");
        assert_eq!(decided(&journal), Vec::new(), "a refusal decides nothing");
    }

    #[test]
    fn a_finished_task_is_left_out_of_the_recovery_pass() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        let candidate = fixture.elsewhere();
        publication_begins(&mut journal, task, 4_000_013, &fixture.base(), &candidate);
        row(
            &mut journal,
            task,
            &EventKind::PublishVerified {
                commit: candidate.clone(),
                remote_sha: candidate.clone(),
            },
        );
        row(
            &mut journal,
            task,
            &EventKind::TaskDone {
                commit: candidate.clone(),
            },
        );

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a finished pass runs");

        assert!(decisions.is_empty(), "{decisions:?}");
        assert_eq!(
            projected(&journal, task),
            Some(TaskState::Done),
            "a finished task stays finished, however many passes run over it"
        );
    }

    #[test]
    fn the_decider_refuses_a_task_that_had_already_finished() {
        let error = decide(
            TaskId::new(1),
            &TaskState::Cancelled,
            &Facts::default(),
            &Checkout::Absent,
            &dead,
            &never_asked,
        )
        .expect_err("a terminal task is not a phase to resolve");

        assert!(matches!(error, Error::Corrupt { .. }), "{error}");
    }

    #[test]
    fn every_decision_is_journalled_beside_the_task_it_describes() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let checks = TaskId::new(1);
        let crashed = TaskId::new(2);
        journal.put_tasks(&queue(2)).expect("a queue is importable");
        preflight_begins(&mut journal, checks);
        attempt_begins(&mut journal, crashed, 1, 4_000_014, &fixture.base());

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead, &mainline())
            .expect("a mixed pass runs");

        let rows = decided(&journal);
        let expected: Vec<(Option<TaskId>, Recovery, String)> = decisions
            .iter()
            .map(|one| (one.task, one.decision, one.detail.clone()))
            .collect();
        assert_eq!(rows, expected, "every verdict is journaled");
        assert_eq!(
            decisions
                .iter()
                .map(|one| (one.task, one.decision))
                .collect::<Vec<_>>(),
            vec![
                (Some(checks), Recovery::Resume),
                (Some(crashed), Recovery::MarkInterrupted),
            ]
        );
        assert!(
            decisions[1].detail.contains("4000014"),
            "the evidence names the process it asked about: {}",
            decisions[1].detail
        );
    }
}

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
//! Three inputs are read, in that order, and nothing else is. The events are folded
//! into the state each task is in — not read out of `task_state`, which ADR-0023 made
//! disposable precisely so a crash could be answered from the events. The kernel is
//! asked about the `pid` the journal recorded, with [`crate::lock`]'s one
//! implementation of that question. And git is asked which checkouts the repository
//! registers, because the commit a publication was making either exists or does not.
//!
//! What recovery never does is change the world: it fetches nothing, commits nothing,
//! pushes nothing, creates and removes no worktree, and never takes the repository
//! lock. Every one of those side effects belongs to a command an operator started.
//! The whole of ADR-0096 is the rule that turns these three answers into one of
//! §6's three verdicts, and the combinations it refuses rather than guesses about.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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
/// holds no repository, because the checkout is one of the three inputs and guessing
/// about the other two is not recovery.
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
    reconcile_with(journal, project, &life_of)
}

/// The clause every verdict carries when no attempt's process was asked about,
/// because the state the task is in names no attempt in flight.
const NO_PROCESS: &str = "no process was asked about, because no attempt of this task is in flight";

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
) -> Result<Vec<RecoveryDecision>> {
    let folded = journal.replayed_states()?;
    let projected = journal.all_states()?;
    let drift = drifted_rows(&projected, &folded);
    let mut facts = read_facts(journal)?;
    let checkouts = read_checkouts(&project.root)?;
    let mut states = folded.clone();
    let mut decided: Vec<RecoveryDecision> = Vec::new();
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
        let (decision, detail) = decide(task, &state, &record, &checkout, process)?;
        let moved = apply(
            &state,
            &EventKind::RecoveryDecision {
                decision,
                detail: detail.clone(),
            },
        )?;
        states.insert(task, moved);
        decided.push(RecoveryDecision {
            task: Some(task),
            decision,
            detail,
        });
    }
    check_one_active(&states)?;
    let repaired = drift.into_iter().map(|detail| RecoveryDecision {
        task: None,
        decision: Recovery::AlreadyApplied,
        detail,
    });
    let verdicts = repaired.chain(decided).collect::<Vec<_>>();
    for verdict in &verdicts {
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
    Ok(verdicts)
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
) -> Result<(Recovery, String)> {
    match state {
        TaskState::Queued => from_queue(task, state, facts, checkout),
        TaskState::Preflight => Ok((
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
                "preflight is the supervisor's own checks and nothing an agent can corrupt, so they are simply run again",
            ),
        )),
        TaskState::Running { attempt, .. } | TaskState::Remediating { attempt, .. } => {
            agents(task, *attempt, state, facts, checkout, process)
        }
        TaskState::Verifying { attempt } => gates(task, *attempt, state, facts, checkout, process),
        TaskState::Publishing { attempt } => {
            publication(task, *attempt, state, facts, checkout, process)
        }
        TaskState::PublishedVerified { .. } => Ok((
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
                "the remote was read back holding the commit, which no restart can unprove, so the publication is not made a second time",
            ),
        )),
        TaskState::Paused { .. } => Ok((
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
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
) -> Result<(Recovery, String)> {
    match checkout {
        Checkout::Absent => Ok((
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                NO_PROCESS,
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
    Ok((decision, evidence(state, facts, checkout, &asked, because)))
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
    Ok((decision, evidence(state, facts, checkout, &asked, because)))
}

/// The publication: the one phase whose side effect outlives the process.
///
/// A commit is either there or it is not, and the checkout says which. The commit
/// the journal offered sitting in `HEAD` means the publication happened and must
/// not happen again; the attempt's own base commit still in `HEAD` means it did
/// not happen and may be run again. Anything else is a commit no row ever offered,
/// which is not a verdict to choose but a repository to be asked about.
fn publication(
    task: TaskId,
    attempt: AttemptId,
    state: &TaskState,
    facts: &Facts,
    checkout: &Checkout,
    process: &dyn Fn(u32) -> Life,
) -> Result<(Recovery, String)> {
    let started = claim(facts, task, attempt, state)?;
    let (runs, asked) = liveness(started.pid, process);
    if runs {
        return Ok((
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                "the push is still in the air, so it is waited for rather than started again",
            ),
        ));
    }
    let Checkout::At { path, head } = checkout else {
        return Ok((
            Recovery::MarkInterrupted,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                "the checkout the commit was to be made in is gone, so nothing was committed and \
                 nothing was pushed",
            ),
        ));
    };
    let offered = facts.offered.get(&attempt);
    if offered == Some(head) {
        return Ok((
            Recovery::AlreadyApplied,
            evidence(
                state,
                facts,
                checkout,
                &asked,
                "the checkout already holds the very commit the journal offered, so the commit \
                 exists and making it again would be a second one",
            ),
        ));
    }
    if head == &started.base_sha {
        return Ok((
            Recovery::Resume,
            evidence(
                state,
                facts,
                checkout,
                &asked,
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
    because: &str,
) -> String {
    format!(
        "state {}{}; {}; {}; the journal's last row is {}; {}",
        state.name(),
        whereof(state),
        asked,
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
    use crate::recovery::{Checkout, Facts, RecoveryDecision, decide, reconcile, reconcile_with};
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
        reconcile_with(journal, &fixture.project, &|pid| {
            probes.set(probes.get() + 1);
            answer(pid)
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

    #[test]
    fn a_projection_that_disagrees_with_the_journal_is_repaired_and_that_is_recorded() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let task = TaskId::new(1);
        preflight_begins(&mut journal, task);
        journal
            .put_state(task, &TaskState::Done)
            .expect("a projection row is writable");

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a matching pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a queued pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("an untouched pass runs");

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

        let error = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a preflight pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a crashed pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a remediation pass runs");

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

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let decisions = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a mid-push pass runs");

        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "{decisions:?}"
        );
        assert!(
            decisions[0].detail.contains(&candidate),
            "the verdict names the commit it refused to make again: {}",
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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a pre-commit pass runs");

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

        let error = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a vanished pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a published pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a parked pass runs");

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
        reconcile_with(&mut journal, &fixture.project, &dead).expect("the first pass runs");
        let parked = projected(&journal, task);

        let second =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("the second pass runs");

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

        let error = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let error = reconcile_with(&mut journal, &fixture.project, &dead)
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

        let error = reconcile_with(&mut journal, &project, &dead)
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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a finished pass runs");

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

        let decisions =
            reconcile_with(&mut journal, &fixture.project, &dead).expect("a mixed pass runs");

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

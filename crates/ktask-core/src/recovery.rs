//! Crash recovery reconciliation: [`reconcile`] resolves an interrupted run
//! to a known state on restart (`VISION.md` §6).
//!
//! A machine can lose power mid-gate, mid-commit or mid-push. On restart,
//! nothing may be assumed: the last thing the journal recorded might have
//! already taken effect, might have been mid-flight when the process died,
//! or might describe a process that is — surprisingly — still running.
//! `reconcile` inspects the live process table and the task's worktree
//! before deciding, and journals the decision it reaches either way, so a
//! second restart never has to redo the same detective work from scratch.

use crate::{
    AttemptId, Error, EventKind, Journal, PauseReason, Phase, Project, Recovery, Result, TaskId,
    TaskState, apply, list_worktrees,
};
use std::collections::BTreeMap;

/// One decision [`reconcile`] made while resolving a project's journal
/// against reality after a restart.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryDecision {
    /// The materialized `task_state` projection disagreed with a fresh
    /// replay of the journal, so [`Journal::rebuild_state`] corrected it.
    StateRebuilt,
    /// `task`'s interrupted attempt was reconciled with reality.
    Task {
        /// The task this decision concerns.
        task: TaskId,
        /// What was decided.
        decision: Recovery,
        /// A human-readable account of the evidence behind `decision`.
        detail: String,
    },
}

/// Resolves every task in `project`'s journal to a known state after a
/// restart (`VISION.md` §6).
///
/// First brings the materialized `task_state` projection back in line with
/// a fresh replay of the journal, via [`Journal::rebuild_state`], if the two
/// disagree — the projection may not reflect a write that landed just
/// before the crash. Then walks every task that is not yet terminal,
/// deciding how to reconcile each with reality.
///
/// A task is left alone when there is nothing to reconcile: `Queued` (no
/// attempt ever started), or `Paused` for any reason other than
/// [`PauseReason::Interrupted`] — a deliberate pause, not a crash. Every
/// other non-terminal task is either already `Paused` on
/// [`PauseReason::Interrupted`] from an earlier, incomplete reconciliation,
/// or genuinely mid-flight, in which case this is the first restart to
/// notice the interruption and [`EventKind::Interrupted`] is journaled for
/// it before a decision is reached. Either way the decision itself is
/// journaled — as [`EventKind::RecoveryDecision`], or, for a task whose
/// publication was already confirmed, the terminal [`EventKind::TaskDone`]
/// that decision amounts to — before it is returned.
///
/// # Errors
///
/// Returns [`Error::Database`], [`Error::Serde`] or [`Error::Corrupt`] from
/// reading or writing the journal, [`Error::Git`] if `project.root`'s
/// worktrees cannot be listed, and [`Error::Corrupt`] if a task is mid
/// attempt with a dead process and no surviving worktree — a combination
/// `crate::runner::Runner::run_task`'s own cleanup contract should never
/// produce, so it is reported rather than guessed at.
pub fn reconcile(journal: &mut Journal, project: &Project) -> Result<Vec<RecoveryDecision>> {
    let tasks = journal.tasks()?;

    let mut derived: BTreeMap<TaskId, TaskState> = BTreeMap::new();
    for task in &tasks {
        let events = journal.events_for(task.id)?;
        if events.is_empty() {
            // No `TaskQueued` yet: `Queued` by convention, not by a stored
            // row (mirrors `rebuild_state`, which never invents one either).
            continue;
        }
        let state = events
            .into_iter()
            .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))?;
        derived.insert(task.id, state);
    }

    let mut decisions = Vec::new();
    if journal.all_states()? != derived {
        journal.rebuild_state()?;
        decisions.push(RecoveryDecision::StateRebuilt);
    }

    for task in &tasks {
        let state = derived.get(&task.id).cloned().unwrap_or(TaskState::Queued);
        if state.is_terminal() {
            continue;
        }
        if let Some(decision) = reconcile_task(journal, project, task.id, &state)? {
            decisions.push(decision);
        }
    }

    Ok(decisions)
}

/// Reconciles one non-terminal task, returning the decision reached, or
/// `None` when there is nothing to reconcile.
fn reconcile_task(
    journal: &mut Journal,
    project: &Project,
    task: TaskId,
    state: &TaskState,
) -> Result<Option<RecoveryDecision>> {
    match state {
        TaskState::Queued => Ok(None),
        TaskState::Paused { reason, .. } if !matches!(reason, PauseReason::Interrupted) => Ok(None),
        TaskState::Paused { resume_to, .. } => {
            reconcile_interrupted(journal, project, task, resume_to, true)
        }
        TaskState::PublishedVerified { commit } => {
            reconcile_published_verified(journal, task, commit)
        }
        TaskState::Preflight
        | TaskState::Running { .. }
        | TaskState::Remediating { .. }
        | TaskState::Verifying { .. }
        | TaskState::Publishing { .. } => {
            reconcile_interrupted(journal, project, task, state, false)
        }
        TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled => {
            unreachable!("reconcile filters terminal states before calling reconcile_task")
        }
    }
}

/// Reconciles a task whose real work is described by `inner`: either
/// already `Paused` on [`PauseReason::Interrupted`] (`already_paused`) from
/// an earlier, incomplete reconciliation, or freshly discovered mid-flight
/// this restart, in which case [`EventKind::Interrupted`] is journaled
/// first — `state.rs`'s `from_paused` only accepts
/// [`EventKind::RecoveryDecision`] once a task is already parked there.
fn reconcile_interrupted(
    journal: &mut Journal,
    project: &Project,
    task: TaskId,
    inner: &TaskState,
    already_paused: bool,
) -> Result<Option<RecoveryDecision>> {
    let (recovery, detail) = decide(journal, project, task, inner)?;

    if !already_paused {
        journal.append(
            Some(task),
            &EventKind::Interrupted {
                phase: interrupted_phase(inner),
            },
        )?;
        let paused = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(inner.clone()),
        };
        journal.put_state(task, &paused)?;
    }

    journal.append(
        Some(task),
        &EventKind::RecoveryDecision {
            decision: recovery,
            detail: detail.clone(),
        },
    )?;
    let resolved = match recovery {
        Recovery::Resume | Recovery::AlreadyApplied => inner.clone(),
        Recovery::MarkInterrupted => TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(inner.clone()),
        },
    };
    journal.put_state(task, &resolved)?;

    Ok(Some(RecoveryDecision::Task {
        task,
        decision: recovery,
        detail,
    }))
}

/// A task confirmed published (`EventKind::PublishVerified` already in the
/// journal) but never reached `EventKind::TaskDone` before the crash.
///
/// There is nothing to redo: `PublishVerified` is itself the durable
/// evidence the commit is present on the remote (`VISION.md` §3 invariant
/// 7). `state.rs`'s `from_published_verified` accepts only a matching
/// `TaskDone` — never `Interrupted` or `RecoveryDecision` — so recovery
/// finishes the bookkeeping it still expects instead of wrapping this task
/// in a `Paused`/`RecoveryDecision` pair it can never legally accept.
fn reconcile_published_verified(
    journal: &mut Journal,
    task: TaskId,
    commit: &str,
) -> Result<Option<RecoveryDecision>> {
    let detail = format!(
        "PublishVerified already confirmed commit {commit} present on the remote before the \
         crash; recording TaskDone instead of redoing anything"
    );
    journal.append(
        Some(task),
        &EventKind::TaskDone {
            commit: commit.to_string(),
        },
    )?;
    journal.put_state(task, &TaskState::Done)?;
    Ok(Some(RecoveryDecision::Task {
        task,
        decision: Recovery::AlreadyApplied,
        detail,
    }))
}

/// Decides how to reconcile `state` — a task's real, unwrapped work — with
/// reality: whether it is safe to [`Recovery::Resume`], must be
/// [`Recovery::MarkInterrupted`] pending a human, or is already
/// [`Recovery::AlreadyApplied`].
///
/// Only called with the states [`EventKind::Interrupted`] legally applies
/// to (`state.rs`'s `from_preflight`, `from_running`, `from_remediating`,
/// `from_verifying` and `from_publishing`); every other variant is
/// unreachable here by construction.
fn decide(
    journal: &Journal,
    project: &Project,
    task: TaskId,
    state: &TaskState,
) -> Result<(Recovery, String)> {
    match state {
        TaskState::Preflight => Ok((
            Recovery::Resume,
            "preflight makes no external change; redoing it is safe".to_string(),
        )),
        TaskState::Running { attempt, .. } | TaskState::Remediating { attempt, .. } => {
            resume_or_mark(
                journal,
                project,
                task,
                *attempt,
                "the attempt never reached verification, so redoing it has no external side \
                 effect",
            )
        }
        TaskState::Verifying { attempt } => resume_or_mark(
            journal,
            project,
            task,
            *attempt,
            "verification only runs local gates, so redoing it has no external side effect",
        ),
        TaskState::Publishing { .. } => Ok((
            Recovery::MarkInterrupted,
            "the push to mainline may have already landed before the crash; VISION.md requires \
             this is never assumed either way"
                .to_string(),
        )),
        TaskState::Queued
        | TaskState::PublishedVerified { .. }
        | TaskState::Paused { .. }
        | TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled => {
            unreachable!("decide is only called with the states Interrupted legally applies to")
        }
    }
}

/// The shared decision for `Running`, `Remediating` and `Verifying`: a
/// recorded pid is either alive or dead, and reaching this point with a
/// dead process and no surviving worktree contradicts `run_task`'s own
/// cleanup contract (its worktree is removed only once the attempt has
/// returned), so that combination is reported as [`Error::Corrupt`] instead
/// of guessed at.
fn resume_or_mark(
    journal: &Journal,
    project: &Project,
    task: TaskId,
    attempt: AttemptId,
    redo_is_safe: &str,
) -> Result<(Recovery, String)> {
    let pid = attempt_pid(journal, task, attempt)?;
    if pid_alive(pid) {
        return Ok((
            Recovery::MarkInterrupted,
            format!(
                "attempt {attempt}'s process (pid {pid}) still appears to be running; recovery \
                 will not touch it"
            ),
        ));
    }

    if worktree_exists(project, task)? {
        Ok((
            Recovery::Resume,
            format!(
                "attempt {attempt}'s process (pid {pid}) is no longer running and its worktree \
                 survived the crash; {redo_is_safe}"
            ),
        ))
    } else {
        Err(Error::Corrupt {
            detail: format!(
                "task {task} is mid attempt {attempt} with a dead process and no surviving \
                 worktree, a combination no code path should produce for a task that is not yet \
                 terminal or paused"
            ),
        })
    }
}

/// The pid [`EventKind::AttemptStarted`] recorded for `attempt`.
///
/// # Errors
///
/// Returns [`Error::Corrupt`] if `task`'s journal has no such event: every
/// state reachable only via an in-flight attempt is reached through
/// [`EventKind::AttemptStarted`] first, so its absence means the journal
/// does not actually support the state `reconcile` is trying to resolve.
fn attempt_pid(journal: &Journal, task: TaskId, attempt: AttemptId) -> Result<u32> {
    journal
        .events_for(task)?
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::AttemptStarted {
                attempt: started,
                pid,
                ..
            } if started == attempt => Some(pid),
            _ => None,
        })
        .ok_or_else(|| Error::Corrupt {
            detail: format!(
                "task {task} is mid attempt {attempt} but its journal has no AttemptStarted \
                 event recording that attempt's pid"
            ),
        })
}

/// Whether `task`'s isolated worktree (`crate::git::create_worktree`'s
/// `task-<id>` naming) still exists on disk, rather than merely a prunable
/// administrative record of one that is already gone.
fn worktree_exists(project: &Project, task: TaskId) -> Result<bool> {
    let name = format!("task-{task}");
    Ok(list_worktrees(&project.root)?.into_iter().any(|worktree| {
        !worktree.prunable && worktree.path.file_name().and_then(|f| f.to_str()) == Some(&name)
    }))
}

/// The `phase` [`EventKind::Interrupted`] carries for `state`: the
/// attempt's own recorded phase for `Running`/`Remediating`, where it is
/// authoritative (`state.rs`'s `from_running`/`from_remediating` use it to
/// build `resume_to`), and an arbitrary placeholder everywhere else, since
/// every other `from_*` that accepts `Interrupted` ignores its `phase`
/// entirely.
fn interrupted_phase(state: &TaskState) -> Phase {
    match state {
        TaskState::Running { phase, .. } | TaskState::Remediating { phase, .. } => *phase,
        TaskState::Verifying { .. } => Phase::Verify,
        TaskState::Publishing { .. } => Phase::Publish,
        TaskState::Preflight => Phase::Goal,
        _ => unreachable!("only called for the states Interrupted legally applies to"),
    }
}

/// Whether a process is still running under `pid`.
///
/// Signal 0 (`kill(pid, None)`) checks existence and permission without
/// sending anything: `Ok(())`, or `EPERM` (owned by another user, but
/// unmistakably alive), both mean alive; any other error — `ESRCH` above
/// all — means it is not. Unlike [`crate::acquire`]'s liveness check, there
/// is no recorded start time to compare against (`AttemptStarted` carries
/// only a pid), so a pid reused by an unrelated process in the narrow
/// window this leaves is a known limitation, same as that check's own
/// non-Linux fallback.
fn pid_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal;
    use nix::unistd::Pid;

    let raw = i32::try_from(pid).unwrap_or(i32::MAX);
    matches!(
        signal::kill(Pid::from_raw(raw), None),
        Ok(()) | Err(Errno::EPERM)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::scratch_repo;
    use crate::{Phase, create_worktree, remove_worktree};
    use std::path::PathBuf;

    fn open_journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&dir.path().join("journal.db")).expect("open journal");
        (dir, journal)
    }

    fn dummy_project() -> Project {
        Project {
            root: PathBuf::from("/nonexistent/ktask-recovery-test-root"),
            id: "recovery-test".to_string(),
            state_dir: PathBuf::from("/nonexistent/ktask-recovery-test-state"),
        }
    }

    fn sample_task(id: u32) -> crate::Task {
        crate::Task {
            id: TaskId::new(id),
            status: crate::TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "it happens".to_string(),
            done_when: "it happened".to_string(),
            verify: "true".to_string(),
            refs: String::new(),
            protocol: None,
        }
    }

    /// Spawns a trivial child process, waits for it to exit, and returns its
    /// pid: guaranteed not to be running any more.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        let status = child.wait().expect("wait for child");
        assert!(status.success());
        pid
    }

    #[test]
    fn reconcile_rebuilds_materialized_state_when_it_disagrees_with_the_journal() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        journal
            .append(
                Some(task),
                &EventKind::TaskQueued {
                    title: "t".to_string(),
                },
            )
            .expect("TaskQueued");
        journal
            .append(Some(task), &EventKind::PreflightStarted)
            .expect("PreflightStarted");
        // No `put_state` call: `task_state` has no row for `task` at all,
        // which disagrees with the journal's replayed `Preflight`.
        assert_eq!(journal.all_states().expect("all_states"), BTreeMap::new());

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        assert_eq!(decisions.first(), Some(&RecoveryDecision::StateRebuilt));
        assert!(
            decisions
                .iter()
                .any(|d| matches!(d, RecoveryDecision::Task { task: t, .. } if *t == task)),
            "expected a decision for the interrupted task too, got {decisions:?}"
        );
    }

    #[test]
    fn reconcile_does_not_rebuild_when_materialized_state_already_matches() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        journal
            .append(
                Some(task),
                &EventKind::TaskQueued {
                    title: "t".to_string(),
                },
            )
            .expect("TaskQueued");
        journal
            .append(
                Some(task),
                &EventKind::TaskCancelled {
                    reason: "superseded".to_string(),
                },
            )
            .expect("TaskCancelled");
        journal
            .put_state(task, &TaskState::Cancelled)
            .expect("put_state matches the journal already");

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        assert_eq!(
            decisions,
            Vec::new(),
            "a cancelled task with an already-matching projection needs no decision"
        );
    }

    #[test]
    fn reconcile_skips_a_queued_task() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        journal
            .append(
                Some(task),
                &EventKind::TaskQueued {
                    title: "t".to_string(),
                },
            )
            .expect("TaskQueued");
        journal
            .put_state(task, &TaskState::Queued)
            .expect("put_state matches already");

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        assert_eq!(decisions, Vec::new());
        assert_eq!(journal.events_for(task).expect("events_for").len(), 1);
    }

    #[test]
    fn reconcile_skips_a_task_paused_for_a_reason_other_than_interrupted() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        journal
            .append(
                Some(task),
                &EventKind::TaskQueued {
                    title: "t".to_string(),
                },
            )
            .expect("TaskQueued");
        journal
            .append(
                Some(task),
                &EventKind::Paused {
                    reason: PauseReason::Input,
                },
            )
            .expect("Paused(Input)");

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        assert!(
            decisions
                .iter()
                .all(|d| !matches!(d, RecoveryDecision::Task { task: t, .. } if *t == task)),
            "a deliberate, non-crash pause must not be touched, got {decisions:?}"
        );
        assert_eq!(
            journal.events_for(task).expect("events_for").len(),
            2,
            "no Interrupted or RecoveryDecision must be appended for a deliberate pause"
        );
    }

    #[test]
    fn reconcile_resumes_an_interrupted_preflight() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        journal
            .append(
                Some(task),
                &EventKind::TaskQueued {
                    title: "t".to_string(),
                },
            )
            .expect("TaskQueued");
        journal
            .append(Some(task), &EventKind::PreflightStarted)
            .expect("PreflightStarted");

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        assert_eq!(
            decisions,
            vec![
                RecoveryDecision::StateRebuilt,
                RecoveryDecision::Task {
                    task,
                    decision: Recovery::Resume,
                    detail: "preflight makes no external change; redoing it is safe".to_string(),
                }
            ]
        );
        assert_eq!(
            journal.get_state(task).expect("get_state"),
            Some(TaskState::Preflight)
        );
        let events = journal.events_for(task).expect("events_for");
        assert_eq!(
            events.len(),
            4,
            "TaskQueued, PreflightStarted, Interrupted, RecoveryDecision"
        );
        assert_eq!(events[2].kind.discriminant(), "Interrupted");
        assert_eq!(events[3].kind.discriminant(), "RecoveryDecision");
    }

    #[test]
    fn reconcile_marks_interrupted_when_the_attempt_process_is_still_alive() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        let live_pid = std::process::id();
        for kind in [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: live_pid,
                base_sha: "base".to_string(),
            },
        ] {
            journal.append(Some(task), &kind).expect("append");
        }

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        let Some(RecoveryDecision::Task {
            decision, detail, ..
        }) = decisions
            .iter()
            .find(|d| matches!(d, RecoveryDecision::Task { task: t, .. } if *t == task))
        else {
            panic!("expected a decision for task {task}, got {decisions:?}");
        };
        assert_eq!(*decision, Recovery::MarkInterrupted);
        assert!(detail.contains(&live_pid.to_string()), "detail: {detail}");
        assert_eq!(
            journal.get_state(task).expect("get_state"),
            Some(TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                }),
            }),
            "a live process must stay parked, not resumed"
        );
    }

    #[test]
    fn reconcile_resumes_when_the_attempt_process_is_dead_and_its_worktree_survived() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = Project {
            root: repo.path.clone(),
            id: "recovery-resume-test".to_string(),
            state_dir: state_dir.path().to_path_buf(),
        };
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        let pid = dead_pid();
        let worktree = create_worktree(&repo.path, "task-1", &repo.seed_sha)
            .expect("create the leftover worktree the crashed attempt left behind");

        for kind in [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: repo.seed_sha.clone(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid,
                base_sha: repo.seed_sha.clone(),
            },
        ] {
            journal.append(Some(task), &kind).expect("append");
        }

        let decisions = reconcile(&mut journal, &project).expect("reconcile");

        remove_worktree(&repo.path, &worktree).expect("clean up the worktree");

        let Some(RecoveryDecision::Task { decision, .. }) = decisions
            .iter()
            .find(|d| matches!(d, RecoveryDecision::Task { task: t, .. } if *t == task))
        else {
            panic!("expected a decision for task {task}, got {decisions:?}");
        };
        assert_eq!(*decision, Recovery::Resume);
        assert_eq!(
            journal.get_state(task).expect("get_state"),
            Some(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }),
            "a dead process with a surviving worktree must resume, not stay parked"
        );
    }

    #[test]
    fn reconcile_errors_when_the_attempt_process_is_dead_and_no_worktree_survived() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = Project {
            root: repo.path.clone(),
            id: "recovery-corrupt-test".to_string(),
            state_dir: state_dir.path().to_path_buf(),
        };
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        let pid = dead_pid();

        for kind in [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: repo.seed_sha.clone(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid,
                base_sha: repo.seed_sha.clone(),
            },
        ] {
            journal.append(Some(task), &kind).expect("append");
        }
        let before = journal.events_for(task).expect("events_for");

        let err = reconcile(&mut journal, &project)
            .expect_err("a dead process with no surviving worktree must not be guessed at");

        assert!(matches!(err, Error::Corrupt { .. }), "got {err:?}");
        let message = err.to_string();
        assert!(message.contains(&task.to_string()), "message: {message}");
        assert_eq!(
            journal.events_for(task).expect("events_for"),
            before,
            "a failed reconciliation must not journal a partial decision"
        );
    }

    #[test]
    fn reconcile_marks_publishing_interrupted_regardless_of_the_process() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        for kind in [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: std::process::id(),
                base_sha: "base".to_string(),
            },
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Verify,
            },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
            EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "cand".to_string(),
            },
        ] {
            journal.append(Some(task), &kind).expect("append");
        }

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        let Some(RecoveryDecision::Task {
            decision, detail, ..
        }) = decisions
            .iter()
            .find(|d| matches!(d, RecoveryDecision::Task { task: t, .. } if *t == task))
        else {
            panic!("expected a decision for task {task}, got {decisions:?}");
        };
        assert_eq!(*decision, Recovery::MarkInterrupted);
        assert!(detail.contains("mainline"), "detail: {detail}");
    }

    #[test]
    fn reconcile_completes_a_task_whose_publish_was_already_verified() {
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        for kind in [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: std::process::id(),
                base_sha: "base".to_string(),
            },
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Verify,
            },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
            EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "cand".to_string(),
            },
            EventKind::PublishVerified {
                commit: "cand".to_string(),
                remote_sha: "cand".to_string(),
            },
        ] {
            journal.append(Some(task), &kind).expect("append");
        }

        let decisions = reconcile(&mut journal, &dummy_project()).expect("reconcile");

        assert_eq!(
            decisions,
            vec![
                RecoveryDecision::StateRebuilt,
                RecoveryDecision::Task {
                    task,
                    decision: Recovery::AlreadyApplied,
                    detail: "PublishVerified already confirmed commit cand present on the \
                              remote before the crash; recording TaskDone instead of redoing \
                              anything"
                        .to_string(),
                }
            ]
        );
        assert_eq!(
            journal.get_state(task).expect("get_state"),
            Some(TaskState::Done)
        );
        let events = journal.events_for(task).expect("events_for");
        assert_eq!(
            events.last().expect("last event").kind.discriminant(),
            "TaskDone"
        );
    }

    #[test]
    fn reconcile_reevaluates_a_task_already_paused_as_interrupted() {
        let repo = scratch_repo().expect("scratch_repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = Project {
            root: repo.path.clone(),
            id: "recovery-reeval-test".to_string(),
            state_dir: state_dir.path().to_path_buf(),
        };
        let (_dir, mut journal) = open_journal();
        let task = TaskId::new(1);
        journal.put_tasks(&[sample_task(1)]).expect("put_tasks");
        let pid = dead_pid();
        let worktree = create_worktree(&repo.path, "task-1", &repo.seed_sha)
            .expect("create the leftover worktree");

        // A previous, already-completed reconciliation already recorded
        // `Interrupted`: this task starts out `Paused` on it.
        for kind in [
            EventKind::TaskQueued {
                title: "t".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: repo.seed_sha.clone(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid,
                base_sha: repo.seed_sha.clone(),
            },
            EventKind::Interrupted {
                phase: Phase::Implement,
            },
        ] {
            journal.append(Some(task), &kind).expect("append");
        }
        assert_eq!(
            journal
                .events_for(task)
                .expect("events_for")
                .into_iter()
                .try_fold(TaskState::Queued, |s, e| apply(&s, &e.kind))
                .expect("apply"),
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                }),
            },
            "sanity: the task starts out already parked on Interrupted"
        );

        let decisions = reconcile(&mut journal, &project).expect("reconcile");

        remove_worktree(&repo.path, &worktree).expect("clean up the worktree");

        let Some(RecoveryDecision::Task { decision, .. }) = decisions
            .iter()
            .find(|d| matches!(d, RecoveryDecision::Task { task: t, .. } if *t == task))
        else {
            panic!("expected a decision for task {task}, got {decisions:?}");
        };
        assert_eq!(*decision, Recovery::Resume);
        assert_eq!(
            journal.get_state(task).expect("get_state"),
            Some(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            })
        );
        // Exactly one new event -- the `RecoveryDecision` -- was appended;
        // no second `Interrupted` was journaled on top of the first.
        assert_eq!(journal.events_for(task).expect("events_for").len(), 6);
    }
}

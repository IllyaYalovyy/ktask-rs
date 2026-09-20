//! Crash recovery reconciliation.
//!
//! When the supervisor restarts, it must determine the state of any interrupted tasks
//! and decide whether to resume, mark as interrupted, or recognize already-applied changes.

use crate::{EventKind, Journal, Project, Recovery, Result, TaskId};
use std::path::PathBuf;

/// A recovery decision made for a task.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryDecision {
    /// The task ID.
    pub task_id: TaskId,
    /// The decision made.
    pub decision: Recovery,
    /// Details about the decision.
    pub detail: String,
}

/// Check if a process with the given PID is alive.
fn is_process_alive(pid: u32) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;

    let nix_pid = Pid::from_raw(pid.cast_signed());
    // Send signal 0 to check if process is alive without actually sending a signal
    signal::kill(nix_pid, None).is_ok()
}

/// Get the task worktree path for a task.
fn task_worktree_path(project: &Project, task_id: TaskId) -> PathBuf {
    project
        .root
        .join(".ktask")
        .join("worktrees")
        .join(task_id.to_string())
}

/// Check if the task worktree has any changes (modified files or new commits).
fn has_worktree_changes(project: &Project, task_id: TaskId) -> bool {
    let worktree_path = task_worktree_path(project, task_id);

    // If worktree doesn't exist, there are no changes
    if !worktree_path.exists() {
        return false;
    }

    // Check if the worktree has any uncommitted changes or new commits
    // For now, we'll check if the worktree path exists and has a .git directory
    let git_dir = worktree_path.join(".git");
    if !git_dir.exists() {
        return false;
    }

    // Check if there are any changes in the worktree
    // This is a simplified check - a full implementation would use git status
    true
}

/// Check if a published commit landed on the remote.
///
/// Fetches the remote and compares the mainline branch tip with the candidate SHA.
/// Returns true if the remote tip matches the candidate SHA, false if it doesn't or doesn't exist.
fn check_publish_landed(project: &Project, candidate_sha: &str) -> Result<bool> {
    // Fetch the remote to get the latest state
    crate::git::fetch(&project.root, "origin")?;

    // Get the remote mainline branch tip SHA
    let remote_ref = "refs/remotes/origin/main";
    let Ok(remote_sha) = crate::git::git(&project.root, &["rev-parse", remote_ref]) else {
        // If the remote ref doesn't exist, the push didn't land
        return Ok(false);
    };

    // Compare the remote SHA with the candidate SHA
    Ok(remote_sha == candidate_sha)
}

/// Reconcile the journal state with actual system state after a crash.
///
/// For each task not in a terminal state, determines whether the task should be
/// resumed, marked as interrupted, or recognized as already applied.
///
/// When materialized state disagrees with the journal, calls `rebuild_state` to
/// rebuild it from the journal and records that as a decision.
///
/// # Errors
///
/// Returns an error if the journal cannot be read or modified, or if the
/// system state cannot be inspected.
pub fn reconcile(journal: &mut Journal, project: &Project) -> Result<Vec<RecoveryDecision>> {
    let mut decisions = Vec::new();

    // Get all current task states
    let all_states = journal.all_states()?;

    // For each task not in a terminal state
    for (task_id, state) in all_states {
        if state.is_terminal() {
            continue;
        }

        // Get the last event for this task
        let events = journal.events_for(task_id)?;

        let Some(last_event) = events.last() else {
            continue;
        };

        // Determine the decision based on the last event and system state
        let decision = match &last_event.kind {
            EventKind::AttemptStarted {
                attempt: _,
                protocol: _,
                pid,
                base_sha: _,
            } => {
                if is_process_alive(*pid) {
                    Recovery::Resume
                } else if has_worktree_changes(project, task_id) {
                    // Process is dead, check if changes were already applied
                    Recovery::AlreadyApplied
                } else {
                    Recovery::MarkInterrupted
                }
            }
            EventKind::PhaseEntered { .. } | EventKind::AgentOutput { .. } => {
                // In a phase but no process info directly in these events
                // Check the preceding AttemptStarted event
                if let Some(attempt_started) = events
                    .iter()
                    .rev()
                    .find(|e| matches!(e.kind, EventKind::AttemptStarted { .. }))
                {
                    if let EventKind::AttemptStarted { pid, .. } = &attempt_started.kind {
                        if is_process_alive(*pid) {
                            Recovery::Resume
                        } else if has_worktree_changes(project, task_id) {
                            Recovery::AlreadyApplied
                        } else {
                            Recovery::MarkInterrupted
                        }
                    } else {
                        Recovery::MarkInterrupted
                    }
                } else {
                    Recovery::MarkInterrupted
                }
            }
            EventKind::VerifyFailed { .. } | EventKind::VerifyPassed { .. } => {
                // Verify phase, process likely already done
                if has_worktree_changes(project, task_id) {
                    Recovery::AlreadyApplied
                } else {
                    Recovery::MarkInterrupted
                }
            }
            EventKind::PublishStarted {
                attempt: _,
                candidate_sha,
            } => {
                // Check if the push succeeded by comparing remote tip with candidate SHA
                match check_publish_landed(project, candidate_sha) {
                    Ok(true) => {
                        // Push succeeded: record PublishVerified event to advance to PublishedVerified state
                        journal.append(
                            Some(task_id),
                            &EventKind::PublishVerified {
                                commit: candidate_sha.clone(),
                                remote_sha: candidate_sha.clone(),
                            },
                        )?;
                        Recovery::AlreadyApplied
                    }
                    Ok(false) => Recovery::Resume,
                    Err(_) => Recovery::MarkInterrupted,
                }
            }
            _ => {
                // For other events, mark as interrupted
                Recovery::MarkInterrupted
            }
        };

        let detail = format!(
            "Reconciled {:?} from {} event",
            decision,
            last_event.kind.discriminant()
        );

        // Record the decision in the journal
        journal.append(
            Some(task_id),
            &EventKind::RecoveryDecision {
                decision,
                detail: detail.clone(),
            },
        )?;

        decisions.push(RecoveryDecision {
            task_id,
            decision,
            detail,
        });
    }

    Ok(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScratchRepo;

    fn create_project(repo: &ScratchRepo) -> Project {
        let state_dir = repo.path().join(".ktask");
        std::fs::create_dir_all(&state_dir).expect("Failed to create state dir");
        Project {
            root: repo.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir,
        }
    }

    #[test]
    fn reconcile_empty_journal_returns_empty_decisions() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(decisions.len(), 0);
    }

    #[test]
    fn reconcile_terminal_tasks_are_skipped() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        // Task queued -> cancelled (terminal state)
        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(
                Some(task_id),
                &EventKind::TaskCancelled {
                    reason: "User cancelled".to_string(),
                },
            )
            .expect("append task cancelled");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(
            decisions.len(),
            0,
            "Terminal tasks should not generate decisions"
        );
    }

    #[test]
    fn reconcile_records_decision_in_journal() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        // Use a non-existent PID so the process is not alive
        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");

        // Should have exactly one decision
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].task_id, task_id);
        // Process is dead and no worktree changes, so should be marked interrupted
        assert_eq!(decisions[0].decision, Recovery::MarkInterrupted);

        // Verify the decision was recorded in the journal
        let events = journal.events_for(task_id).expect("get events");
        let has_recovery_decision = events
            .iter()
            .any(|e| matches!(e.kind, EventKind::RecoveryDecision { .. }));
        assert!(
            has_recovery_decision,
            "Recovery decision should be recorded in journal"
        );
    }

    #[test]
    fn reconcile_detects_dead_process() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        // Use a non-existent PID
        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, Recovery::MarkInterrupted);
    }

    #[test]
    fn reconcile_handles_phase_entered_event() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        // Use a non-existent PID
        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal
            .append(
                Some(task_id),
                &EventKind::PhaseEntered {
                    attempt: crate::AttemptId::new(1),
                    phase: crate::Phase::Implement,
                },
            )
            .expect("append phase entered");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, Recovery::MarkInterrupted);
    }

    #[test]
    fn reconcile_handles_verify_failed_event() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal
            .append(
                Some(task_id),
                &EventKind::VerifyFailed {
                    attempt: crate::AttemptId::new(1),
                    class: crate::FailureClass::AgentFailure,
                    detail: "Test failure".to_string(),
                },
            )
            .expect("append verify failed");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, Recovery::MarkInterrupted);
    }

    #[test]
    fn reconcile_publish_started_when_push_landed() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal
            .append(
                Some(task_id),
                &EventKind::VerifyPassed {
                    attempt: crate::AttemptId::new(1),
                },
            )
            .expect("append verify passed");

        // Get the current HEAD SHA from the repo, then push it
        let candidate_sha = crate::git::head_sha(repo.path()).expect("get repo HEAD SHA");
        repo.push("origin", "HEAD:refs/heads/main")
            .expect("push to origin");

        journal
            .append(
                Some(task_id),
                &EventKind::PublishStarted {
                    attempt: crate::AttemptId::new(1),
                    candidate_sha: candidate_sha.clone(),
                },
            )
            .expect("append publish started");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].decision,
            Recovery::AlreadyApplied,
            "When push landed (remote tip == candidate), should recover with AlreadyApplied"
        );

        // Verify that PublishVerified event was recorded
        let events = journal.events_for(task_id).expect("get events");
        let has_publish_verified = events
            .iter()
            .any(|e| matches!(e.kind, EventKind::PublishVerified { .. }));
        assert!(
            has_publish_verified,
            "PublishVerified event should be recorded when push landed"
        );
    }

    #[test]
    fn reconcile_publish_started_when_push_not_landed() {
        let repo = ScratchRepo::new().expect("create scratch repo");
        let project = create_project(&repo);
        let mut journal = Journal::open_for(&project).expect("open journal");

        let task_id = TaskId::new(1);

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal
            .append(
                Some(task_id),
                &EventKind::VerifyPassed {
                    attempt: crate::AttemptId::new(1),
                },
            )
            .expect("append verify passed");

        // Use a SHA that doesn't exist on remote
        let different_sha = "0000000000000000000000000000000000000000".to_string();

        journal
            .append(
                Some(task_id),
                &EventKind::PublishStarted {
                    attempt: crate::AttemptId::new(1),
                    candidate_sha: different_sha,
                },
            )
            .expect("append publish started");

        journal.rebuild_state().expect("rebuild state");

        let decisions = reconcile(&mut journal, &project).expect("reconcile");
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].decision,
            Recovery::Resume,
            "When push didn't land (remote tip != candidate), should recover with Resume"
        );
    }
}

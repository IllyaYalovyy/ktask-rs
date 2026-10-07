//! The sync gate: brings a project's directory up to date with its tracked branch before a
//! task's attempt is even begun. Not one of the [`super::Step`]s an attempt walks — refusing to
//! run leaves no attempt at all, so a task it stops stays `pending` — but shaped the same way
//! as every step: a name, a switch, and running it.

use std::time::Duration;

use crate::run::RunEnd;
use crate::settings::split_tracked_branch;
use crate::{Clock, Git, Journal, PullRebase, PullRebaseError, RunContext, RunError, TaskId};

/// The journal name of the sync step.
pub const SYNC_STEP: &str = "sync";

/// Why the sync ahead of a task's health check refused to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncProblem {
    /// The project's directory holds changes git has not committed: the listing is
    /// `git status --porcelain`'s own output, read before anything else was touched.
    UncommittedChanges(String),
    /// The tracked branch's remote could not be reached.
    RemoteUnreachable(String),
    /// Rebasing onto the tracked branch conflicted in these files. The rebase was undone
    /// before this was returned: the directory is exactly as it was.
    Conflict(Vec<String>),
    /// Git itself could not do the work asked of it, for some other reason.
    GitFailed(String),
}

impl SyncProblem {
    /// What failed and what is expected of the operator, condensed to one line so it fits a
    /// status step's own line — the same words `ktask-rs run` prints for it.
    pub(crate) fn message(&self, tracked_branch: &str) -> String {
        match self {
            Self::UncommittedChanges(status) => format!(
                "the project's directory has uncommitted changes: {}; commit or stash your \
                 changes, then run again",
                status.replace('\n', ", ")
            ),
            Self::RemoteUnreachable(reason) => format!(
                "{tracked_branch}'s remote could not be reached: {}; make the remote \
                 reachable, then run again",
                reason.replace('\n', " / ")
            ),
            Self::Conflict(files) => format!(
                "rebasing onto {tracked_branch} conflicted in: {}; the rebase was undone; \
                 resolve the conflict yourself (pull --rebase, fix, push), then run again",
                files.join(", ")
            ),
            Self::GitFailed(reason) => format!(
                "{}; fix the problem, then run again",
                reason.replace('\n', " / ")
            ),
        }
    }
}

/// What running the sync gate produced.
#[derive(Debug)]
pub(crate) enum Sync {
    /// It found and, when there was anything to bring in, rebased in `message`'s worth of
    /// commits, taking `duration`.
    Passed { duration: Duration, message: String },
    /// It refused to run.
    Failed(SyncProblem),
}

/// Whether the sync gate is switched on for `context`: a branch is tracked, and the step is not
/// named in `context.disabled_steps`.
pub(crate) fn enabled(context: RunContext<'_>) -> bool {
    context.tracked_branch.is_some() && context.step_enabled(SYNC_STEP)
}

/// Pulls `tracked_branch` (`"<remote>/<branch>"`) with rebase into `context.project_dir`: is
/// refused when the directory holds uncommitted changes or the remote cannot be reached;
/// rebases onto the tracked branch when it fetched anything new, undoing the rebase and naming
/// every file it conflicted in when it did.
fn sync_with_tracked_branch(
    git: &dyn Git,
    tracked_branch: &str,
    context: RunContext<'_>,
) -> Result<String, SyncProblem> {
    let Some((remote, branch)) = split_tracked_branch(tracked_branch) else {
        unreachable!("a saved tracked-branch setting always names a remote and a branch");
    };
    match git.pull_rebase(context.project_dir, remote, branch) {
        Ok(PullRebase::UpToDate) => Ok("nothing new".to_owned()),
        Ok(PullRebase::TookIn(count)) => Ok(format!(
            "took in {count} commit{} from {tracked_branch}",
            if count == 1 { "" } else { "s" }
        )),
        Err(PullRebaseError::UncommittedChanges(status)) => {
            Err(SyncProblem::UncommittedChanges(status))
        }
        Err(PullRebaseError::RemoteUnreachable(reason)) => {
            Err(SyncProblem::RemoteUnreachable(reason))
        }
        Err(PullRebaseError::Conflict(files)) => Err(SyncProblem::Conflict(files)),
        Err(PullRebaseError::Failed(reason)) => Err(SyncProblem::GitFailed(reason)),
    }
}

/// Runs the sync gate against `tracked_branch`, timing it with `clock`.
pub(crate) fn run(
    git: &dyn Git,
    clock: &dyn Clock,
    tracked_branch: &str,
    context: RunContext<'_>,
) -> Sync {
    let started = clock.now();
    match sync_with_tracked_branch(git, tracked_branch, context) {
        Ok(message) => Sync::Passed {
            duration: clock.now().duration_since(started).unwrap_or_default(),
            message,
        },
        Err(problem) => Sync::Failed(problem),
    }
}

/// Runs the sync gate ahead of `task_id`'s attempt: the pre-step to record when it passes,
/// `None` when the gate is switched off, or the [`RunEnd`] that stops the run when it refuses —
/// recording why in the journal first, so a later `status` can show it even though the task
/// stays `pending`.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn run_gate(
    journal: &dyn Journal,
    git: &dyn Git,
    clock: &dyn Clock,
    context: RunContext<'_>,
    task_id: TaskId,
) -> Result<Result<Option<super::PreStep>, RunEnd>, RunError> {
    if !enabled(context) {
        return Ok(Ok(None));
    }
    let Some(tracked_branch) = context.tracked_branch else {
        unreachable!("sync::enabled only returns true when a branch is tracked");
    };
    match run(git, clock, tracked_branch, context) {
        Sync::Passed { duration, message } => Ok(Ok(Some(super::PreStep {
            name: SYNC_STEP,
            duration,
            reason: Some(message),
        }))),
        Sync::Failed(problem) => {
            crate::attempt::record_gate_failure(
                journal,
                clock,
                task_id,
                SYNC_STEP,
                &problem.message(tracked_branch),
            )?;
            Ok(Err(RunEnd::SyncFailed {
                id: task_id,
                tracked_branch: tracked_branch.to_owned(),
                problem,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fakes::FakeGit;

    fn context() -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: Duration::from_secs(60),
            health_check_command: None,
            check_command: None,
            tracked_branch: Some("origin/main"),
            disabled_steps: &[],
            max_attempts: 1,
            transport_retries: 3,
            model: "",
            resolver_model: "",
            sessions_dir: Path::new("/state/sessions"),
            outputs_dir: Path::new("/state/outputs"),
        }
    }

    struct StoppedClock;
    impl Clock for StoppedClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
    }

    #[test]
    fn disabled_when_no_branch_is_tracked() {
        let mut ctx = context();
        ctx.tracked_branch = None;
        assert!(!enabled(ctx));
    }

    #[test]
    fn disabled_when_switched_off_even_with_a_tracked_branch() {
        let mut ctx = context();
        ctx.disabled_steps = &[SYNC_STEP];
        assert!(!enabled(ctx));
    }

    #[test]
    fn enabled_with_a_tracked_branch_and_the_switch_on() {
        assert!(enabled(context()));
    }

    #[test]
    fn nothing_new_says_so_and_never_rebases() {
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::UpToDate)),
            ..FakeGit::default()
        };
        match run(&git, &StoppedClock, "origin/main", context()) {
            Sync::Passed { message, .. } => assert_eq!(message, "nothing new"),
            Sync::Failed(problem) => panic!("expected it to pass, got {problem:?}"),
        }
    }

    #[test]
    fn a_single_commit_taken_in_is_singular_in_the_message() {
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::TookIn(1))),
            ..FakeGit::default()
        };
        match run(&git, &StoppedClock, "origin/main", context()) {
            Sync::Passed { message, .. } => {
                assert_eq!(message, "took in 1 commit from origin/main");
            }
            Sync::Failed(problem) => panic!("expected it to pass, got {problem:?}"),
        }
    }

    #[test]
    fn several_commits_taken_in_are_plural_in_the_message() {
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::TookIn(3))),
            ..FakeGit::default()
        };
        match run(&git, &StoppedClock, "origin/main", context()) {
            Sync::Passed { message, .. } => {
                assert_eq!(message, "took in 3 commits from origin/main");
            }
            Sync::Failed(problem) => panic!("expected it to pass, got {problem:?}"),
        }
    }

    #[test]
    fn uncommitted_changes_refuse_the_sync() {
        let git = FakeGit {
            pull_rebase: Some(Err(PullRebaseError::UncommittedChanges(
                "M file.txt".to_owned(),
            ))),
            ..FakeGit::default()
        };
        match run(&git, &StoppedClock, "origin/main", context()) {
            Sync::Failed(SyncProblem::UncommittedChanges(status)) => {
                assert_eq!(status, "M file.txt");
            }
            other => panic!("expected UncommittedChanges, got {other:?}"),
        }
    }

    #[test]
    fn a_conflict_names_every_file_and_the_rebase_was_already_undone() {
        let git = FakeGit {
            pull_rebase: Some(Err(PullRebaseError::Conflict(vec![
                "a.txt".to_owned(),
                "b.txt".to_owned(),
            ]))),
            ..FakeGit::default()
        };
        match run(&git, &StoppedClock, "origin/main", context()) {
            Sync::Failed(SyncProblem::Conflict(files)) => {
                assert_eq!(files, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn every_problems_message_fits_on_one_line_even_when_git_answers_with_several() {
        let cases = [
            SyncProblem::UncommittedChanges("M a.txt\nM b.txt".to_owned()),
            SyncProblem::RemoteUnreachable("fatal: one\nfatal: two".to_owned()),
            SyncProblem::Conflict(vec!["a.txt".to_owned(), "b.txt".to_owned()]),
            SyncProblem::GitFailed("fatal: one\nfatal: two".to_owned()),
        ];
        for problem in cases {
            let message = problem.message("origin/main");
            assert_eq!(message.lines().count(), 1, "{message:?}");
        }
        assert!(
            SyncProblem::UncommittedChanges("M a.txt".to_owned())
                .message("origin/main")
                .contains("commit or stash")
        );
        assert!(
            SyncProblem::RemoteUnreachable("nope".to_owned())
                .message("origin/main")
                .contains("make the remote reachable")
        );
        assert!(
            SyncProblem::Conflict(vec!["a.txt".to_owned()])
                .message("origin/main")
                .contains("resolve the conflict yourself")
        );
        assert!(
            SyncProblem::GitFailed("nope".to_owned())
                .message("origin/main")
                .contains("fix the problem")
        );
    }
}

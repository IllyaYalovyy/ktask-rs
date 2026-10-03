//! The push step: once the commit step has made a commit and the project tracks a branch,
//! pushes it and confirms it landed.

use crate::settings::split_tracked_branch;
use crate::steps::{Deps, PipelineState, Step, StepOutcome};
use crate::{Git, PUSH_STEP, PushError, RunContext, RunError, TaskStatus};

/// What pushing the commit step's commit to the project's tracked branch, and confirming the
/// remote holds it, found and did.
enum PushOutcome {
    /// The remote branch's tip is now the commit that was pushed: its short hash.
    Pushed(String),
    /// It could not push, or could not confirm the push landed on the remote: why, and — since
    /// this ends the task `failed` — what is expected of the operator.
    Refused(String),
}

/// Pushes `context`'s project directory's `HEAD` to `tracked_branch` (`"<remote>/<branch>"`),
/// then confirms — with `git ls-remote`, checked live against the remote rather than any
/// locally cached ref — that the branch's tip on the remote is now that commit: a push exiting
/// zero is not itself proof the ref actually moved. Refuses, naming why and what is expected of
/// the operator, when the push itself is rejected or cannot be run, or when the remote's tip
/// does not turn out to match afterwards.
fn push_commit(git: &dyn Git, context: RunContext<'_>, tracked_branch: &str) -> PushOutcome {
    let Some((remote, branch)) = split_tracked_branch(tracked_branch) else {
        unreachable!("a saved tracked-branch setting always names a remote and a branch");
    };
    match git.push_and_confirm(context.project_dir, remote, branch) {
        Ok(hash) => PushOutcome::Pushed(hash),
        Err(PushError::Rejected) => PushOutcome::Refused(format!(
            "{tracked_branch} has moved on since this task's commit was made; bring in the \
             new commits and push it yourself, then run again"
        )),
        Err(PushError::Failed(reason)) => PushOutcome::Refused(reason),
    }
}

/// The push step of a task's attempt.
pub(crate) struct Push;

impl Step for Push {
    fn name(&self) -> &'static str {
        PUSH_STEP
    }

    fn enabled(&self, context: RunContext<'_>, state: &PipelineState<'_>) -> bool {
        context.tracked_branch.is_some()
            && context.step_enabled(PUSH_STEP)
            && state.committed.is_some()
    }

    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        _state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError> {
        let Some(tracked_branch) = context.tracked_branch else {
            unreachable!("enabled() only returns true when a branch is tracked");
        };
        let started = deps.clock.now();
        let outcome = push_commit(deps.git, context, tracked_branch);
        let duration = deps.clock.now().duration_since(started).unwrap_or_default();
        Ok(match outcome {
            PushOutcome::Pushed(hash) => StepOutcome::Passed {
                duration,
                exit_code: Some(0),
                reason: Some(format!("pushed {hash} to {tracked_branch}")),
                reported: None,
            },
            PushOutcome::Refused(why) => StepOutcome::Ended {
                duration,
                exit_code: None,
                status: TaskStatus::Failed,
                reason: Some(why),
                reported: None,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::fakes::{FakeGit, FakeJournal, at};
    use crate::{AttemptToken, Task, TaskId, TaskKind};

    fn context() -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: Duration::from_secs(60),
            health_check_command: None,
            tracked_branch: Some("origin/main"),
            disabled_steps: &[],
            max_attempts: 1,
            resolver_model: "",
            sessions_dir: Path::new("/state/sessions"),
            outputs_dir: Path::new("/state/outputs"),
        }
    }

    fn task() -> Task {
        Task {
            id: TaskId(1),
            position: 1,
            title: "a".to_owned(),
            body: String::new(),
            criteria: vec![],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        }
    }

    struct StoppedClock;
    impl crate::Clock for StoppedClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
    }

    fn run_push(git: &dyn Git, task: &Task) -> StepOutcome {
        let token = AttemptToken::new("proj", task.id, 1);
        let mut state = PipelineState {
            task,
            token: &token,
            start_commit: None,
            committed: Some("abc1234".to_owned()),
            exit_code: None,
            failure: None,
            requested_model: None,
            requested_session: None,
            known_cause: false,
        };
        let commands = crate::fakes::FakeCommands::returning(Ok(crate::Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: crate::Exit::Code(0),
        }));
        let provider = crate::Provider {
            name: "test".to_owned(),
            command: std::sync::Arc::new(|_, _| {
                unreachable!("the push step never runs a provider")
            }),
            supports_resume: false,
            read_session: std::sync::Arc::new(|_| None),
            detect_limit: std::sync::Arc::new(|_| None),
            parse_output: std::sync::Arc::new(|output| output),
        };
        let session_log = crate::fakes::FakeSessionLog::default();
        let sleep = crate::fakes::FakeSleep::default();
        let deps = Deps {
            journal: &FakeJournal::default(),
            clock: &StoppedClock,
            commands: &commands,
            git,
            provider: &provider,
            session_log: &session_log,
            sleep: &sleep,
        };
        Push.run(&deps, context(), &mut state).unwrap()
    }

    #[test]
    fn a_push_confirmed_on_the_remote_records_its_own_line() {
        let git = FakeGit {
            push: Some(Ok("abcdef1".to_owned())),
            ..FakeGit::default()
        };
        match run_push(&git, &task()) {
            StepOutcome::Passed { reason, .. } => {
                assert_eq!(reason, Some("pushed abcdef1 to origin/main".to_owned()));
            }
            StepOutcome::Ended { reason, .. } => panic!("expected Passed, got Ended({reason:?})"),
            StepOutcome::Waiting { .. } => panic!("expected Passed, got Waiting"),
        }
    }

    #[test]
    fn a_push_rejected_because_the_remote_moved_on_says_so() {
        let git = FakeGit {
            push: Some(Err(PushError::Rejected)),
            ..FakeGit::default()
        };
        match run_push(&git, &task()) {
            StepOutcome::Ended { status, reason, .. } => {
                assert_eq!(status, TaskStatus::Failed);
                let reason = reason.unwrap();
                assert!(reason.contains("has moved on"), "{reason}");
                assert!(reason.contains("run again"), "{reason}");
            }
            StepOutcome::Passed { .. } => panic!("expected Ended"),
            StepOutcome::Waiting { .. } => panic!("expected Ended, got Waiting"),
        }
    }

    #[test]
    fn a_refused_push_carries_gits_own_reason() {
        let git = FakeGit {
            push: Some(Err(PushError::Failed(
                "`git push origin HEAD:refs/heads/main` exited with code 128: fatal: could not \
                 read from remote repository"
                    .to_owned(),
            ))),
            ..FakeGit::default()
        };
        match run_push(&git, &task()) {
            StepOutcome::Ended { reason, .. } => {
                let reason = reason.unwrap();
                assert!(reason.contains("git push"), "{reason}");
                assert!(reason.contains("exited with code 128"), "{reason}");
            }
            StepOutcome::Passed { .. } => panic!("expected Ended"),
            StepOutcome::Waiting { .. } => panic!("expected Ended, got Waiting"),
        }
    }

    #[test]
    fn disabled_without_a_tracked_branch() {
        let mut ctx = context();
        ctx.tracked_branch = None;
        let task = task();
        let token = AttemptToken::new("proj", task.id, 1);
        let state = PipelineState {
            task: &task,
            token: &token,
            start_commit: None,
            committed: Some("abc1234".to_owned()),
            exit_code: None,
            failure: None,
            requested_model: None,
            requested_session: None,
            known_cause: false,
        };
        assert!(!Push.enabled(ctx, &state));
    }

    #[test]
    fn disabled_when_no_commit_was_made_even_with_a_tracked_branch() {
        let task = task();
        let token = AttemptToken::new("proj", task.id, 1);
        let state = PipelineState {
            task: &task,
            token: &token,
            start_commit: None,
            committed: None,
            exit_code: None,
            failure: None,
            requested_model: None,
            requested_session: None,
            known_cause: false,
        };
        assert!(!Push.enabled(context(), &state));
    }

    #[test]
    fn disabled_when_switched_off() {
        let mut ctx = context();
        ctx.disabled_steps = &[PUSH_STEP];
        let task = task();
        let token = AttemptToken::new("proj", task.id, 1);
        let state = PipelineState {
            task: &task,
            token: &token,
            start_commit: None,
            committed: Some("abc1234".to_owned()),
            exit_code: None,
            failure: None,
            requested_model: None,
            requested_session: None,
            known_cause: false,
        };
        assert!(!Push.enabled(ctx, &state));
    }
}

//! The commit step: once the steps ahead of it have passed, commits everything the task's
//! attempt changed.

use crate::steps::{Deps, PipelineState, Step, StepOutcome};
use crate::{CommitAllError, Git, RunContext, RunError, Task, TaskStatus};

/// The journal name of the commit step.
pub const COMMIT_STEP: &str = "commit";

/// Why the commit step refuses when the project directory has changes but git has not been
/// told whose they are, and what is expected of the operator because of it.
const IDENTITY_NOT_CONFIGURED: &str = "git identity is not configured: set it with `git config \
    user.name \"Your Name\"` and `git config user.email you@example.com`, then run again";

/// What committing everything the attempt changed, once its test step has passed, found and
/// did.
enum CommitOutcome {
    /// Nothing in the project directory had changed: no commit was made.
    NothingChanged,
    /// A commit was made: its short hash.
    Committed(String),
    /// It could not commit: why, and — since this ends the task `failed` — what is expected of
    /// the operator.
    Refused(String),
}

/// The message the commit step gives its commit: `task`'s title as the subject, then its ID
/// and acceptance criteria as the body. Carries no trailer of any kind — no co-author line, no
/// tool or model named — so the commit reads as the user's own.
fn build_commit_message(task: &Task) -> String {
    let mut message = format!("{}\n\nTask #{}\n", task.title, task.id);
    if !task.criteria.is_empty() {
        message.push_str("\nAcceptance criteria:\n");
        for criterion in &task.criteria {
            message.push_str("- ");
            message.push_str(criterion);
            message.push('\n');
        }
    }
    message
}

/// Commits everything changed in `context`'s project directory, for `task`, under whatever
/// identity git is configured with there — author and committer both, since neither is ever
/// overridden. Makes no commit, refusing nothing, when nothing had changed. Refuses, changing
/// nothing, when the identity is not configured or git itself refuses the commit.
fn commit_everything_changed(git: &dyn Git, context: RunContext<'_>, task: &Task) -> CommitOutcome {
    let message = build_commit_message(task);
    match git.commit_all(context.project_dir, &message) {
        Ok(Some(hash)) => CommitOutcome::Committed(hash),
        Ok(None) => CommitOutcome::NothingChanged,
        Err(CommitAllError::IdentityNotConfigured) => {
            CommitOutcome::Refused(IDENTITY_NOT_CONFIGURED.to_owned())
        }
        Err(CommitAllError::Failed(reason)) => CommitOutcome::Refused(reason),
    }
}

/// The commit step of a task's attempt.
pub(crate) struct Commit;

impl Step for Commit {
    fn name(&self) -> &'static str {
        COMMIT_STEP
    }

    fn enabled(&self, context: RunContext<'_>, _state: &PipelineState<'_>) -> bool {
        context.step_enabled(COMMIT_STEP)
    }

    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError> {
        let started = deps.clock.now();
        let outcome = commit_everything_changed(deps.git, context, state.task);
        let duration = deps.clock.now().duration_since(started).unwrap_or_default();
        Ok(match outcome {
            CommitOutcome::NothingChanged => StepOutcome::Passed {
                duration,
                exit_code: Some(0),
                reason: Some("nothing was changed".to_owned()),
                reported: None,
            },
            CommitOutcome::Committed(hash) => {
                state.committed = Some(hash.clone());
                StepOutcome::Passed {
                    duration,
                    exit_code: Some(0),
                    reason: Some(format!("committed as {hash}")),
                    reported: None,
                }
            }
            CommitOutcome::Refused(why) => StepOutcome::Ended {
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
    use crate::fakes::{FakeGit, at};
    use crate::{TaskId, TaskKind};

    #[test]
    fn the_commit_message_carries_the_title_id_and_criteria_and_no_trailer() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: "ignored here".to_owned(),
            criteria: vec!["first thing".to_owned(), "second thing".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let message = build_commit_message(&task);
        assert!(message.starts_with("Do the thing\n\n"), "{message}");
        assert!(message.contains("Task #7"), "{message}");
        assert!(message.contains("- first thing"), "{message}");
        assert!(message.contains("- second thing"), "{message}");
        let lower = message.to_lowercase();
        assert!(!lower.contains("co-authored-by"), "{message}");
        assert!(!lower.contains("claude"), "{message}");
        assert!(!lower.contains("generated"), "{message}");
    }

    fn context() -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: Duration::from_secs(60),
            health_check_command: None,
            tracked_branch: None,
            disabled_steps: &[],
            max_attempts: 1,
            model: "",
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

    fn run_commit(git: &dyn Git, task: &Task) -> StepOutcome {
        let token = crate::AttemptToken::new("proj", task.id, 1);
        let mut state = PipelineState {
            task,
            token: &token,
            start_commit: None,
            committed: None,
            exit_code: None,
            failure: None,
            requested_model: None,
            requested_session: None,
            known_cause: false,
            usage: crate::Usage::default(),
            used_model: None,
            limit_warning: None,
        };
        let commands = crate::fakes::FakeCommands::returning(Ok(crate::Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: crate::Exit::Code(0),
        }));
        let provider = crate::Provider {
            name: "test".to_owned(),
            command: std::sync::Arc::new(|_, _| {
                unreachable!("the commit step never runs a provider")
            }),
            supports_resume: false,
            read_session: std::sync::Arc::new(|_| None),
            detect_limit: std::sync::Arc::new(|_| None),
            parse_output: std::sync::Arc::new(|output| output),
            read_usage: std::sync::Arc::new(|_| crate::ProviderUsage::default()),
        };
        let session_log = crate::fakes::FakeSessionLog::default();
        let sleep = crate::fakes::FakeSleep::default();
        let deps = Deps {
            journal: &crate::fakes::FakeJournal::default(),
            clock: &StoppedClock,
            commands: &commands,
            git,
            provider: &provider,
            resolver_provider: &provider,
            session_log: &session_log,
            sleep: &sleep,
        };
        Commit.run(&deps, context(), &mut state).unwrap()
    }

    #[test]
    fn a_dirty_tree_with_a_configured_identity_is_committed_and_records_its_short_hash() {
        let git = FakeGit {
            commit_all: Some(Ok(Some("abc1234".to_owned()))),
            ..FakeGit::default()
        };
        let task = task();
        match run_commit(&git, &task) {
            StepOutcome::Passed { reason, .. } => {
                assert_eq!(reason, Some("committed as abc1234".to_owned()));
            }
            StepOutcome::Ended { reason, .. } => panic!("expected Passed, got Ended({reason:?})"),
            StepOutcome::Waiting { .. } => panic!("expected Passed, got Waiting"),
        }
    }

    #[test]
    fn a_clean_tree_makes_no_commit_and_says_so() {
        let git = FakeGit {
            commit_all: Some(Ok(None)),
            ..FakeGit::default()
        };
        let task = task();
        match run_commit(&git, &task) {
            StepOutcome::Passed { reason, .. } => {
                assert_eq!(reason, Some("nothing was changed".to_owned()));
            }
            StepOutcome::Ended { reason, .. } => panic!("expected Passed, got Ended({reason:?})"),
            StepOutcome::Waiting { .. } => panic!("expected Passed, got Waiting"),
        }
    }

    #[test]
    fn an_unconfigured_identity_refuses_the_commit() {
        let git = FakeGit {
            commit_all: Some(Err(CommitAllError::IdentityNotConfigured)),
            ..FakeGit::default()
        };
        let task = task();
        match run_commit(&git, &task) {
            StepOutcome::Ended { status, reason, .. } => {
                assert_eq!(status, TaskStatus::Failed);
                let reason = reason.unwrap();
                assert!(
                    reason.contains("git identity is not configured"),
                    "{reason}"
                );
                assert!(reason.contains("user.name"), "{reason}");
                assert!(reason.contains("user.email"), "{reason}");
            }
            StepOutcome::Passed { .. } => panic!("expected Ended"),
            StepOutcome::Waiting { .. } => panic!("expected Ended, got Waiting"),
        }
    }

    #[test]
    fn a_refused_commit_carries_gits_own_reason() {
        let git = FakeGit {
            commit_all: Some(Err(CommitAllError::Failed(
                "`git commit` exited with code 1: the pre-commit hook refused it".to_owned(),
            ))),
            ..FakeGit::default()
        };
        let task = task();
        match run_commit(&git, &task) {
            StepOutcome::Ended { status, reason, .. } => {
                assert_eq!(status, TaskStatus::Failed);
                let reason = reason.unwrap();
                assert!(reason.contains("git commit"), "{reason}");
                assert!(reason.contains("exited with code 1"), "{reason}");
                assert!(
                    reason.contains("the pre-commit hook refused it"),
                    "{reason}"
                );
            }
            StepOutcome::Passed { .. } => panic!("expected Ended"),
            StepOutcome::Waiting { .. } => panic!("expected Ended, got Waiting"),
        }
    }

    #[test]
    fn disabled_when_switched_off() {
        let mut ctx = context();
        ctx.disabled_steps = &[COMMIT_STEP];
        let task = task();
        let token = crate::AttemptToken::new("proj", task.id, 1);
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
            usage: crate::Usage::default(),
            used_model: None,
            limit_warning: None,
        };
        assert!(!Commit.enabled(ctx, &state));
    }
}

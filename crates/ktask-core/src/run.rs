//! `run`: takes the pending tasks in queue order and attempts each one, once, with the
//! [`Provider`] it is given. Takes the run lock, accounts for a run an earlier kill left an
//! attempt running in, picks the next task, and walks it through the steps of
//! [`crate::steps`] — the sync and health-check gates ahead of an attempt, then the list an
//! attempt walks.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::pick::{Pick, end_when_nothing_left, pick_next_task};
use crate::steps::{self, INTERRUPTED};
use crate::{
    BeginAttemptError, Clock, Commands, Git, Journal, JournalError, Provider, RecordReportError,
    RunLock, RunLockError, TaskId, TaskStatus,
};

pub use crate::steps::implementation::build_prompt;
pub use crate::steps::review::build_review_prompt;
pub use crate::steps::sync::SyncProblem;
pub use crate::steps::test_step::build_test_prompt;

/// Why a run could not proceed at all — never for how an attempt itself ended, which is a
/// normal [`RunReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// Another run already holds the project's run lock.
    Locked(RunLockError),
    /// Some other failure — the journal could not be read or written.
    Other(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked(error) => error.fmt(f),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl Error for RunError {}

impl From<JournalError> for RunError {
    fn from(error: JournalError) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<BeginAttemptError> for RunError {
    fn from(error: BeginAttemptError) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<RecordReportError> for RunError {
    fn from(error: RecordReportError) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<RunLockError> for RunError {
    fn from(error: RunLockError) -> Self {
        Self::Locked(error)
    }
}

/// One task the run attempted, and how its one attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempted {
    /// The task attempted.
    pub id: TaskId,
    /// What the task ended at: `done`, `failed`, `blocked` or `failed-unknown`.
    pub status: TaskStatus,
    /// Why, when the status is not `done`.
    pub reason: Option<String>,
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// The queue holds no tasks at all.
    EmptyQueue,
    /// Every task in the queue is already done, failed, blocked, failed-unknown or
    /// cancelled: none is pending.
    NothingPending,
    /// The next pending task is kind `human`: the run stops there without attempting it.
    HumanTask(TaskId),
    /// Every pending task was attempted and reported done; none is left pending.
    Completed,
    /// An attempt ended in `status`, other than `done`, and the run stopped.
    Stopped {
        /// The task the run stopped at.
        id: TaskId,
        /// What it ended at: `failed`, `blocked` or `failed-unknown`.
        status: TaskStatus,
    },
    /// The next task in queue order already ended `failed`, `blocked` or `failed-unknown`,
    /// from an earlier run: this run refuses to skip past it and starts nothing.
    Blocked {
        /// The task the run refuses to skip past.
        id: TaskId,
        /// What it ended at: `failed`, `blocked` or `failed-unknown`.
        status: TaskStatus,
        /// Why, from its last attempt.
        reason: Option<String>,
    },
    /// The next task's health check failed, so no attempt was begun for it: it stays
    /// `pending`.
    HealthCheckFailed {
        /// The task the health check ran ahead of.
        id: TaskId,
        /// The command that was run.
        command: String,
        /// Why it failed.
        reason: String,
        /// The end of its combined standard output and standard error.
        output_tail: String,
    },
    /// The next task's sync, ahead of its health check, refused to run it: it stays
    /// `pending`.
    SyncFailed {
        /// The task the sync ran ahead of.
        id: TaskId,
        /// The tracked branch, as configured: `"<remote>/<branch>"`.
        tracked_branch: String,
        /// What went wrong.
        problem: SyncProblem,
    },
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Every task attempted, in the order attempted.
    pub attempted: Vec<Attempted>,
    /// Why the run ended.
    pub end: RunEnd,
}

/// Where a run executes: the project's name, carried in every attempt token, the directory
/// its provider's commands run in, the path of the `ktask-rs` binary that is running it, and
/// how long one attempt may run before it is killed.
#[derive(Debug, Clone, Copy)]
pub struct RunContext<'a> {
    /// The project's name.
    pub project_name: &'a str,
    /// The directory the provider's commands run in.
    pub project_dir: &'a Path,
    /// The path of the running `ktask-rs` binary, so the prompt tells an agent exactly which
    /// one to call back into, whatever is or is not on its `PATH`.
    pub binary_path: &'a Path,
    /// How long one attempt may run before it, and everything it started, is killed.
    pub attempt_timeout: Duration,
    /// The project's configured health-check command, run in `project_dir` before a task's
    /// implementation step, subject to `attempt_timeout` the same way. `None` when the
    /// project has not set one: the step is skipped, and leaves no line.
    pub health_check_command: Option<&'a str>,
    /// The project's configured tracked branch, as `"<remote>/<branch>"`, pulled with rebase
    /// in `project_dir` ahead of the health check. `None` when the project has not set one:
    /// the step is skipped, and leaves no line.
    pub tracked_branch: Option<&'a str>,
    /// The steps the project has switched off, named as [`crate::SYNC_STEP`],
    /// [`crate::HEALTH_CHECK_STEP`], [`crate::REVIEW_STEP`], [`crate::TEST_STEP`],
    /// [`crate::COMMIT_STEP`] or [`crate::PUSH_STEP`] — never [`crate::IMPLEMENTATION`], which
    /// cannot be switched off. A step named here does not run and leaves no line, whatever
    /// else is configured for it; the sync and health-check steps still only actually run when
    /// `tracked_branch`, respectively `health_check_command`, is also set, and the push step
    /// only when the commit step made a commit.
    pub disabled_steps: &'a [&'static str],
}

impl RunContext<'_> {
    /// Whether the step named `step` is switched on: named in `disabled_steps` or not.
    pub(crate) fn step_enabled(&self, step: &str) -> bool {
        !self.disabled_steps.contains(&step)
    }
}

/// Takes `lock` for the whole run, so that two runs of the same project never overlap.
///
/// # Errors
///
/// Fails when another run already holds `lock`.
fn take_lock(lock: &impl RunLock) -> Result<(), RunError> {
    lock.acquire()?;
    Ok(())
}

/// Accounts for a previous run that was killed while an attempt was in progress: that
/// attempt's task is left `running` in the journal with no attempt-ended event. Ends it
/// `failed-unknown` with the reason [`INTERRUPTED`] and returns it. Returns `None` when no
/// task was left running.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn account_for_interrupted_run(
    journal: &impl Journal,
    clock: &impl Clock,
) -> Result<Option<Attempted>, RunError> {
    let Some((id, number)) = crate::attempt::running(journal)? else {
        return Ok(None);
    };
    crate::attempt::end_attempt(
        journal,
        id,
        number,
        crate::AttemptRun {
            duration: Duration::ZERO,
            exit_code: None,
            status: TaskStatus::FailedUnknown,
            reason: Some(INTERRUPTED),
        },
        clock.now(),
    )?;
    Ok(Some(Attempted {
        id,
        status: TaskStatus::FailedUnknown,
        reason: Some(INTERRUPTED.to_owned()),
    }))
}

/// Runs the sync gate ahead of `task_id`'s attempt: the pre-step to record when it passes,
/// `None` when not enabled, or the [`RunEnd`] that stops the run when it refuses.
fn run_sync_gate(
    git: &dyn Git,
    clock: &dyn Clock,
    context: RunContext<'_>,
    task_id: TaskId,
) -> Result<Option<steps::PreStep>, RunEnd> {
    if !steps::sync::enabled(context) {
        return Ok(None);
    }
    let Some(tracked_branch) = context.tracked_branch else {
        unreachable!("sync::enabled only returns true when a branch is tracked");
    };
    match steps::sync::run(git, clock, tracked_branch, context) {
        steps::sync::Sync::Passed { duration, message } => Ok(Some(steps::PreStep {
            name: crate::SYNC_STEP,
            duration,
            reason: Some(message),
        })),
        steps::sync::Sync::Failed(problem) => Err(RunEnd::SyncFailed {
            id: task_id,
            tracked_branch: tracked_branch.to_owned(),
            problem,
        }),
    }
}

/// Runs the health-check gate ahead of `task_id`'s attempt: the pre-step to record when it
/// passes, `None` when not enabled, or the [`RunEnd`] that stops the run when it fails.
fn run_health_check_gate(
    commands: &dyn Commands,
    clock: &dyn Clock,
    context: RunContext<'_>,
    task_id: TaskId,
) -> Result<Option<steps::PreStep>, RunEnd> {
    if !steps::health_check::enabled(context) {
        return Ok(None);
    }
    let Some(command) = context.health_check_command else {
        unreachable!("health_check::enabled only returns true when a command is configured");
    };
    match steps::health_check::run(commands, clock, command, context) {
        steps::health_check::HealthCheck::Passed(duration) => Ok(Some(steps::PreStep {
            name: crate::HEALTH_CHECK_STEP,
            duration,
            reason: None,
        })),
        steps::health_check::HealthCheck::Failed {
            reason,
            output_tail,
        } => Err(RunEnd::HealthCheckFailed {
            id: task_id,
            command: command.to_owned(),
            reason,
            output_tail,
        }),
    }
}

/// Runs `task`'s sync and health-check gates, then its one attempt, appending its result to
/// `attempted`. `Ok(Some(end))` when the run stops here — a gate refused, or the attempt did
/// not report `done` — `Ok(None)` to carry on to the next task.
fn attempt_task(
    deps: steps::Deps<'_>,
    context: RunContext<'_>,
    task: &crate::Task,
    attempted: &mut Vec<Attempted>,
) -> Result<Option<RunEnd>, RunError> {
    let mut pre_steps = Vec::new();
    match run_sync_gate(deps.git, deps.clock, context, task.id) {
        Ok(step) => pre_steps.extend(step),
        Err(end) => return Ok(Some(end)),
    }
    match run_health_check_gate(deps.commands, deps.clock, context, task.id) {
        Ok(step) => pre_steps.extend(step),
        Err(end) => return Ok(Some(end)),
    }
    let result = steps::run_one_attempt(deps, context, task, &pre_steps, &steps::default_steps())?;
    let status = result.status;
    attempted.push(result);
    Ok((status != TaskStatus::Done).then_some(RunEnd::Stopped {
        id: task.id,
        status,
    }))
}

/// Picks and attempts pending tasks, one at a time, until the queue stops the run: a task of
/// kind `human`, an attempt that does not report `done`, an earlier task already left
/// `failed`, `blocked` or `failed-unknown`, or nothing left pending. Ahead of each attempt,
/// runs the sync and health-check gates of [`crate::steps`] when the project has configured
/// and switched them on; either refusing to run stops the run before an attempt is even
/// begun, leaving the task `pending`.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn attempt_loop(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    git: &impl Git,
    provider: &Provider,
    context: RunContext<'_>,
) -> Result<RunReport, RunError> {
    let deps = steps::Deps {
        journal,
        clock,
        commands,
        git,
        provider,
    };
    let mut attempted = Vec::new();
    loop {
        match pick_next_task(journal)? {
            Pick::Task(task) => {
                let end = attempt_task(deps, context, &task, &mut attempted)?;
                if let Some(end) = end {
                    return Ok(RunReport { attempted, end });
                }
            }
            Pick::Human(id) => {
                let end = RunEnd::HumanTask(id);
                return Ok(RunReport { attempted, end });
            }
            Pick::Blocked { id, status, reason } => {
                let end = RunEnd::Blocked { id, status, reason };
                return Ok(RunReport { attempted, end });
            }
            Pick::NothingLeft { queue_is_empty } => {
                let end = end_when_nothing_left(&attempted, queue_is_empty);
                return Ok(RunReport { attempted, end });
            }
        }
    }
}

/// Use case: runs the pending tasks of `context.project_name`, in queue order, one attempt
/// each, with `provider` — stopping at the first task of kind `human`, at the first attempt
/// that does not report `done`, at the first task in queue order already left `failed`,
/// `blocked` or `failed-unknown` (nothing is attempted in that case), or when nothing is
/// left pending.
///
/// Takes `lock` for the whole run, so that two runs of the same project never overlap. When
/// the previous run was killed while an attempt was in progress, this run finds its task
/// still `running`, ends it `failed-unknown` with the reason "the run was interrupted", and
/// stops there without attempting anything else.
///
/// # Errors
///
/// Fails, attempting nothing, when another run already holds `lock`. Fails when the journal
/// cannot be read or written; an attempt's own failure is reported in the returned
/// [`RunReport`], not here.
pub fn run_queue(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    git: &impl Git,
    provider: &Provider,
    lock: &impl RunLock,
    context: RunContext<'_>,
) -> Result<RunReport, RunError> {
    take_lock(lock)?;
    if let Some(attempted) = account_for_interrupted_run(journal, clock)? {
        let end = RunEnd::Stopped {
            id: attempted.id,
            status: attempted.status,
        };
        return Ok(RunReport {
            attempted: vec![attempted],
            end,
        });
    }
    attempt_loop(journal, clock, commands, git, provider, context)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::time::SystemTime;

    use crate::fakes::{FakeClock, FakeCommands, FakeGit, FakeJournal, FakeRunLock, at, draft};
    use crate::{
        AttemptRun, AttemptToken, COMMIT_STEP, CommandSpec, Commands, CommandsError,
        CommitAllError, Event, Exit, HEALTH_CHECK_STEP, IMPLEMENTATION, Outcome, Output, PUSH_STEP,
        Placement, Provider, ProviderCommand, PullRebase, PullRebaseError, REVIEW_STEP, SYNC_STEP,
        TEST_STEP, TaskDraft, TaskId, TaskKind, TaskStatus, add_task, report,
    };

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(1_000))
    }

    /// A provider whose command carries the token as `args[1]` (after a placeholder at
    /// `args[0]`, mirroring what a real provider's own flags might occupy), the attempt as
    /// `args[2]`, and the step as `args[3]`, and the prompt as its standard input — enough for
    /// tests to see what `run` passed it, without this being any particular real provider.
    fn test_provider() -> Provider {
        Provider {
            name: "test",
            command: |prompt, call| {
                Ok(ProviderCommand {
                    program: "run-it".to_owned(),
                    args: vec![
                        "-s".to_owned(),
                        call.token.to_owned(),
                        call.attempt.to_string(),
                        call.step.to_owned(),
                    ],
                    stdin: prompt.as_bytes().to_vec(),
                })
            },
        }
    }

    fn commands_ok(exit: Exit) -> FakeCommands {
        FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit,
        }))
    }

    fn context(timeout: Duration) -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: timeout,
            health_check_command: None,
            tracked_branch: None,
            disabled_steps: &[],
        }
    }

    fn run(
        journal: &FakeJournal,
        commands: &impl Commands,
        provider: &Provider,
        timeout: Duration,
    ) -> Result<RunReport, RunError> {
        run_queue(
            journal,
            &clock(),
            commands,
            &FakeGit::default(),
            provider,
            &FakeRunLock::free(),
            context(timeout),
        )
    }

    #[test]
    fn an_empty_queue_ends_the_run_at_once() {
        let journal = FakeJournal::default();
        let commands = commands_ok(Exit::Code(0));
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::EmptyQueue,
            }
        );
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_queue_with_nothing_pending_ends_the_run_without_attempting_anything() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let number = crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            number,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            clock().0,
        )
        .unwrap();
        let commands = commands_ok(Exit::Code(0));
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::NothingPending,
            }
        );
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_human_task_stops_the_run_without_attempting_it() {
        let journal = FakeJournal::default();
        add_task(
            &journal,
            &clock(),
            &TaskDraft {
                kind: TaskKind::Human,
                ..draft("a")
            },
            Placement::End,
        )
        .unwrap();
        let commands = commands_ok(Exit::Code(0));
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::HumanTask(TaskId(1)),
            }
        );
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Pending
        );
        assert!(commands.last.borrow().is_none());
    }

    /// A journal with three pending tasks, `a`, `b` and `c`.
    fn journal_of_abc() -> FakeJournal {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        journal
    }

    /// A fake commands port that, once run, reports `outcome` for whatever token it finds
    /// among its args, then exits with `exit` — except for the review step, which it always
    /// approves, and the test step, which it always accepts.
    struct ReportingCommands<'a> {
        journal: &'a FakeJournal,
        outcome: Outcome,
        exit: Exit,
    }

    impl Commands for ReportingCommands<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            let token: AttemptToken = spec
                .args
                .iter()
                .find_map(|arg| arg.parse().ok())
                .expect("one arg is the attempt token");
            let step = crate::attempt::current_step(self.journal, token.task).unwrap();
            let outcome = match step.as_deref() {
                Some(REVIEW_STEP) => Outcome::Approved,
                Some(TEST_STEP) => Outcome::Accepted,
                _ => self.outcome,
            };
            report(
                self.journal,
                &FakeClock(SystemTime::UNIX_EPOCH),
                &token,
                outcome,
                Some("because"),
            )
            .unwrap();
            Ok(Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: self.exit,
            })
        }
    }

    /// A commands port that answers the health check's own `bash -c` call with
    /// `health_check`, and everything else — the provider's own command — with `other`.
    struct HealthCheckAnd<'a> {
        health_check: Result<Output, CommandsError>,
        other: &'a dyn Commands,
    }

    impl Commands for HealthCheckAnd<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            if spec.program == "bash" && spec.args.first().map(String::as_str) == Some("-c") {
                self.health_check.clone()
            } else {
                self.other.run(spec)
            }
        }
    }

    /// A commands port that panics if it is ever asked to run anything — proves the provider
    /// is never started once the health check has failed.
    struct NeverRun;

    impl Commands for NeverRun {
        fn run(&self, _: &CommandSpec) -> Result<Output, CommandsError> {
            panic!("no command should have run once the health check failed");
        }
    }

    #[test]
    fn a_passing_health_check_is_recorded_as_the_attempts_first_step_and_it_carries_on() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = HealthCheckAnd {
            health_check: Ok(Output {
                stdout: b"all good\n".to_vec(),
                stderr: Vec::new(),
                exit: Exit::Code(0),
            }),
            other: &reporting,
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.health_check_command = Some("make check");
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 5, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, HEALTH_CHECK_STEP);
        assert_eq!(
            attempt.steps[0].ended.as_ref().unwrap().status,
            TaskStatus::Done
        );
        assert_eq!(attempt.steps[1].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[2].name, REVIEW_STEP);
        assert_eq!(
            attempt.steps[2].ended.as_ref().unwrap().reported,
            Some(Outcome::Approved)
        );
        assert_eq!(attempt.steps[3].name, TEST_STEP);
        assert_eq!(
            attempt.steps[3].ended.as_ref().unwrap().reported,
            Some(Outcome::Accepted)
        );
        assert_eq!(attempt.steps[4].name, COMMIT_STEP);
        assert_eq!(
            attempt.steps[4].ended.as_ref().unwrap().status,
            TaskStatus::Done
        );
    }

    #[test]
    fn a_failing_health_check_stops_before_any_attempt_and_leaves_the_task_pending() {
        let journal = journal_of_abc();
        let commands = HealthCheckAnd {
            health_check: Ok(Output {
                stdout: b"building...\n".to_vec(),
                stderr: b"ERROR: nope\n".to_vec(),
                exit: Exit::Code(1),
            }),
            other: &NeverRun,
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.health_check_command = Some("make check");
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::HealthCheckFailed {
                    id: TaskId(1),
                    command: "make check".to_owned(),
                    reason: "the health check exited with code 1".to_owned(),
                    output_tail: "building...\nERROR: nope".to_owned(),
                },
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Pending);
        assert_eq!(
            crate::attempt::last_attempt(&journal, TaskId(1)).unwrap(),
            None
        );
    }

    #[test]
    fn a_health_check_past_its_time_limit_is_killed_and_counts_as_failing() {
        let journal = journal_of_abc();
        let commands = HealthCheckAnd {
            health_check: Ok(Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: Exit::Killed,
            }),
            other: &NeverRun,
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.health_check_command = Some("sleep 999");
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        match report.end {
            RunEnd::HealthCheckFailed { reason, .. } => {
                assert!(reason.contains("time limit"), "{reason}");
            }
            other => panic!("expected HealthCheckFailed, got {other:?}"),
        }
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Pending
        );
    }

    #[test]
    fn the_health_check_runs_in_the_projects_directory_with_the_attempts_timeout() {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Code(1));
        let mut ctx = context(Duration::from_secs(42));
        ctx.health_check_command = Some("make check");
        run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        let spec = commands.last.borrow().clone().unwrap();
        assert_eq!(spec.program, "bash");
        assert_eq!(spec.args, vec!["-c".to_owned(), "make check".to_owned()]);
        assert_eq!(spec.dir, Path::new("/work/proj"));
        assert_eq!(spec.timeout, Duration::from_secs(42));
    }

    #[test]
    fn no_health_check_command_set_skips_the_step() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 4, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[1].name, REVIEW_STEP);
        assert_eq!(attempt.steps[2].name, TEST_STEP);
        assert_eq!(attempt.steps[3].name, COMMIT_STEP);
    }

    #[test]
    fn switching_the_health_check_off_skips_it_even_though_one_is_configured() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.health_check_command = Some("make check");
        ctx.disabled_steps = &[HEALTH_CHECK_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert!(
            !attempt
                .steps
                .iter()
                .any(|step| step.name == HEALTH_CHECK_STEP),
            "{:?}",
            attempt.steps
        );
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[1].name, REVIEW_STEP);
        assert_eq!(attempt.steps[2].name, TEST_STEP);
        assert_eq!(attempt.steps[3].name, COMMIT_STEP);
    }

    #[test]
    fn switching_sync_off_skips_it_even_though_a_branch_is_tracked() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let mut ctx = context_tracking(Duration::from_secs(60));
        ctx.disabled_steps = &[SYNC_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert!(
            !attempt.steps.iter().any(|step| step.name == SYNC_STEP),
            "{:?}",
            attempt.steps
        );
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
    }

    #[test]
    fn switching_review_off_skips_it_and_the_others_still_run_in_order() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.disabled_steps = &[REVIEW_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let names: Vec<_> = attempt
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect();
        assert_eq!(names, [IMPLEMENTATION, TEST_STEP, COMMIT_STEP]);
    }

    #[test]
    fn switching_testing_off_skips_it_and_the_others_still_run_in_order() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.disabled_steps = &[TEST_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let names: Vec<_> = attempt
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect();
        assert_eq!(names, [IMPLEMENTATION, REVIEW_STEP, COMMIT_STEP]);
    }

    #[test]
    fn switching_review_and_testing_off_leaves_only_implementation_and_commit() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.disabled_steps = &[REVIEW_STEP, TEST_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let names: Vec<_> = attempt
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect();
        assert_eq!(names, [IMPLEMENTATION, COMMIT_STEP]);
    }

    /// [`RunContext`] tracking `origin/main`, on top of `context`.
    fn context_tracking(timeout: Duration) -> RunContext<'static> {
        let mut ctx = context(timeout);
        ctx.tracked_branch = Some("origin/main");
        ctx
    }

    #[test]
    fn new_commits_on_the_tracked_branch_are_rebased_in_and_recorded_first() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::TookIn(3))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 5, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, SYNC_STEP);
        let synced = attempt.steps[0].ended.as_ref().unwrap();
        assert_eq!(synced.status, TaskStatus::Done);
        assert_eq!(
            synced.reason.as_deref(),
            Some("took in 3 commits from origin/main")
        );
        assert_eq!(attempt.steps[1].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[2].name, REVIEW_STEP);
        assert_eq!(attempt.steps[3].name, TEST_STEP);
        assert_eq!(attempt.steps[4].name, COMMIT_STEP);
    }

    #[test]
    fn one_new_commit_is_singular_in_the_message() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::TookIn(1))),
            ..FakeGit::default()
        };
        run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            attempt.steps[0].ended.as_ref().unwrap().reason.as_deref(),
            Some("took in 1 commit from origin/main")
        );
    }

    #[test]
    fn nothing_new_says_so_and_never_rebases() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::UpToDate)),
            ..FakeGit::default()
        };
        run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps[0].name, SYNC_STEP);
        assert_eq!(
            attempt.steps[0].ended.as_ref().unwrap().reason.as_deref(),
            Some("nothing new")
        );
        assert!(*git.pull_rebase_calls.borrow() >= 1);
    }

    #[test]
    fn uncommitted_changes_stop_the_sync_before_anything_else_runs_and_the_task_stays_pending() {
        let journal = journal_of_abc();
        let git = FakeGit {
            pull_rebase: Some(Err(PullRebaseError::UncommittedChanges(
                "M file.txt".to_owned(),
            ))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &NeverRun,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::SyncFailed {
                    id: TaskId(1),
                    tracked_branch: "origin/main".to_owned(),
                    problem: SyncProblem::UncommittedChanges("M file.txt".to_owned()),
                },
            }
        );
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Pending
        );
    }

    #[test]
    fn an_unreachable_remote_stops_the_sync_and_the_task_stays_pending() {
        let journal = journal_of_abc();
        let git = FakeGit {
            pull_rebase: Some(Err(PullRebaseError::RemoteUnreachable(
                "fatal: could not read from remote repository".to_owned(),
            ))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &NeverRun,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        match report.end {
            RunEnd::SyncFailed {
                id,
                tracked_branch,
                problem: SyncProblem::RemoteUnreachable(reason),
            } => {
                assert_eq!(id, TaskId(1));
                assert_eq!(tracked_branch, "origin/main");
                assert!(reason.contains("could not read from remote"), "{reason}");
            }
            other => panic!("expected SyncFailed/RemoteUnreachable, got {other:?}"),
        }
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Pending
        );
    }

    #[test]
    fn a_rebase_conflict_is_undone_and_names_every_conflicting_file() {
        let journal = journal_of_abc();
        let git = FakeGit {
            pull_rebase: Some(Err(PullRebaseError::Conflict(vec![
                "file.txt".to_owned(),
                "other.txt".to_owned(),
            ]))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &NeverRun,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::SyncFailed {
                    id: TaskId(1),
                    tracked_branch: "origin/main".to_owned(),
                    problem: SyncProblem::Conflict(vec![
                        "file.txt".to_owned(),
                        "other.txt".to_owned()
                    ]),
                },
            }
        );
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Pending
        );
    }

    #[test]
    fn no_tracked_branch_set_skips_the_sync_step() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 4, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[1].name, REVIEW_STEP);
        assert_eq!(attempt.steps[2].name, TEST_STEP);
        assert_eq!(attempt.steps[3].name, COMMIT_STEP);
    }

    #[test]
    fn the_sync_runs_ahead_of_the_health_check() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = HealthCheckAnd {
            health_check: Ok(Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: Exit::Code(0),
            }),
            other: &reporting,
        };
        let git = FakeGit {
            pull_rebase: Some(Ok(PullRebase::UpToDate)),
            ..FakeGit::default()
        };
        let mut ctx = context_tracking(Duration::from_secs(60));
        ctx.health_check_command = Some("make check");
        run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 6, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, SYNC_STEP);
        assert_eq!(attempt.steps[1].name, HEALTH_CHECK_STEP);
        assert_eq!(attempt.steps[2].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[3].name, REVIEW_STEP);
        assert_eq!(attempt.steps[4].name, TEST_STEP);
        assert_eq!(attempt.steps[5].name, COMMIT_STEP);
    }

    #[test]
    fn three_tasks_that_all_report_done_all_end_done() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![
                    Attempted {
                        id: TaskId(1),
                        status: TaskStatus::Done,
                        reason: None
                    },
                    Attempted {
                        id: TaskId(2),
                        status: TaskStatus::Done,
                        reason: None
                    },
                    Attempted {
                        id: TaskId(3),
                        status: TaskStatus::Done,
                        reason: None
                    },
                ],
                end: RunEnd::Completed,
            }
        );
        for task in crate::list_all_tasks(&journal).unwrap() {
            assert_eq!(task.status, TaskStatus::Done, "{}", task.id);
        }
    }

    #[test]
    fn a_second_different_provider_value_can_run_the_queue_too() {
        let journal = journal_of_abc();
        // A provider unlike `test_provider`: a different program, a different argument
        // layout (the token first, no placeholder flag), and a name of its own — proving
        // `run` works with any [`Provider`] value, not one it recognizes by name.
        let other = Provider {
            name: "other",
            command: |_prompt, call| {
                Ok(ProviderCommand {
                    program: "printf".to_owned(),
                    args: vec![call.token.to_owned(), format!("attempt={}", call.attempt)],
                    stdin: Vec::new(),
                })
            },
        };
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let report = run(&journal, &commands, &other, Duration::from_secs(60)).unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(report.attempted.len(), 3);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.provider.as_deref(), Some("other"));
    }

    #[test]
    fn a_failed_report_ends_that_task_failed_and_stops_the_run_leaving_the_rest_pending() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Failed,
            exit: Exit::Code(0),
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![Attempted {
                    id: TaskId(1),
                    status: TaskStatus::Failed,
                    reason: Some("because".to_owned()),
                }],
                end: RunEnd::Stopped {
                    id: TaskId(1),
                    status: TaskStatus::Failed,
                },
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Failed);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
        assert_eq!(tasks[2].status, TaskStatus::Pending);
    }

    #[test]
    fn a_too_large_report_ends_the_task_failed_too() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::TooLarge,
            exit: Exit::Code(0),
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report.attempted,
            vec![Attempted {
                id: TaskId(1),
                status: TaskStatus::Failed,
                reason: Some("because".to_owned()),
            }]
        );
    }

    #[test]
    fn a_needs_input_report_ends_that_task_blocked_and_stops_the_run() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::NeedsInput,
            exit: Exit::Code(0),
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![Attempted {
                    id: TaskId(1),
                    status: TaskStatus::Blocked,
                    reason: Some("because".to_owned()),
                }],
                end: RunEnd::Stopped {
                    id: TaskId(1),
                    status: TaskStatus::Blocked,
                },
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Blocked);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    /// A commands port that reports `Outcome::Done` for the implementation step and `review`
    /// (outcome and reason) for the review step, then exits with `exit` — proving what the
    /// review step's own outcome does to the task, once it is reached.
    struct ReviewCommands<'a> {
        journal: &'a FakeJournal,
        review: (Outcome, &'static str),
        exit: Exit,
    }

    impl Commands for ReviewCommands<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            let token: AttemptToken = spec
                .args
                .iter()
                .find_map(|arg| arg.parse().ok())
                .expect("one arg is the attempt token");
            let is_review = crate::attempt::current_step(self.journal, token.task)
                .unwrap()
                .as_deref()
                == Some(REVIEW_STEP);
            let (outcome, reason) = if is_review {
                self.review
            } else {
                (Outcome::Done, "because")
            };
            report(
                self.journal,
                &FakeClock(SystemTime::UNIX_EPOCH),
                &token,
                outcome,
                Some(reason),
            )
            .unwrap();
            Ok(Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: self.exit,
            })
        }
    }

    #[test]
    fn changes_requested_ends_the_task_failed_with_the_findings_as_the_reason_and_stops_the_run() {
        let journal = journal_of_abc();
        let commands = ReviewCommands {
            journal: &journal,
            review: (Outcome::ChangesRequested, "fix the thing"),
            exit: Exit::Code(0),
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![Attempted {
                    id: TaskId(1),
                    status: TaskStatus::Failed,
                    reason: Some("fix the thing".to_owned()),
                }],
                end: RunEnd::Stopped {
                    id: TaskId(1),
                    status: TaskStatus::Failed,
                },
            }
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 2, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[1].name, REVIEW_STEP);
        let end = attempt.steps[1].ended.as_ref().unwrap();
        assert_eq!(end.status, TaskStatus::Failed);
        assert_eq!(end.reported, Some(Outcome::ChangesRequested));
        assert_eq!(end.reason.as_deref(), Some("fix the thing"));
        // The task's own status is `failed`, and the next task never started.
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Failed);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    /// A commands port that reports `Outcome::Done` for the implementation step, then exits
    /// cleanly for the review step without ever calling `report` at all — a reviewer that ran
    /// and said nothing.
    struct SilentReview<'a> {
        journal: &'a FakeJournal,
    }

    impl Commands for SilentReview<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            let token: AttemptToken = spec
                .args
                .iter()
                .find_map(|arg| arg.parse().ok())
                .expect("one arg is the attempt token");
            let is_review = crate::attempt::current_step(self.journal, token.task)
                .unwrap()
                .as_deref()
                == Some(REVIEW_STEP);
            if !is_review {
                report(
                    self.journal,
                    &FakeClock(SystemTime::UNIX_EPOCH),
                    &token,
                    Outcome::Done,
                    None,
                )
                .unwrap();
            }
            Ok(Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: Exit::Code(0),
            })
        }
    }

    #[test]
    fn a_reviewer_that_reports_nothing_ends_the_task_failed_unknown_as_implementation_would() {
        let journal = journal_of_abc();
        let commands = SilentReview { journal: &journal };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(report.attempted.len(), 1);
        assert_eq!(report.attempted[0].status, TaskStatus::FailedUnknown);
        assert!(
            report.attempted[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("reported nothing"),
            "{:?}",
            report.attempted[0].reason
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps[1].name, REVIEW_STEP);
        assert_eq!(attempt.steps[1].ended.as_ref().unwrap().reported, None);
    }

    /// A commands port that reports `Outcome::Done` for the implementation step,
    /// `Outcome::Approved` for the review step and `Outcome::Accepted` for the test step,
    /// capturing the prompt (its standard input) it is run with for whichever step is named
    /// `capture_step`, in `captured`.
    struct CapturingStep<'a> {
        journal: &'a FakeJournal,
        capture_step: &'static str,
        captured: &'a RefCell<Option<Vec<u8>>>,
    }

    impl Commands for CapturingStep<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            let token: AttemptToken = spec
                .args
                .iter()
                .find_map(|arg| arg.parse().ok())
                .expect("one arg is the attempt token");
            let step = crate::attempt::current_step(self.journal, token.task).unwrap();
            if step.as_deref() == Some(self.capture_step) {
                *self.captured.borrow_mut() = Some(spec.stdin.clone());
            }
            let outcome = match step.as_deref() {
                Some(REVIEW_STEP) => Outcome::Approved,
                Some(TEST_STEP) => Outcome::Accepted,
                _ => Outcome::Done,
            };
            report(
                self.journal,
                &FakeClock(SystemTime::UNIX_EPOCH),
                &token,
                outcome,
                None,
            )
            .unwrap();
            Ok(Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: Exit::Code(0),
            })
        }
    }

    /// [`FakeGit`] with `head` and `diff` configured to a baseline commit and the diff since
    /// it, as `the_review_step_runs_on_the_diff...` and `the_test_step_runs_on_the_diff...`
    /// need.
    fn git_with_diff() -> FakeGit {
        FakeGit {
            head: Some("abc123".to_owned()),
            diff: "--- a/file\n+++ b/file\n+added line\n".to_owned(),
            ..FakeGit::default()
        }
    }

    #[test]
    fn the_review_step_runs_on_the_diff_the_git_port_reports_since_the_attempt_began() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let captured = RefCell::default();
        let commands = CapturingStep {
            journal: &journal,
            capture_step: REVIEW_STEP,
            captured: &captured,
        };
        run_queue(
            &journal,
            &clock(),
            &commands,
            &git_with_diff(),
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(60)),
        )
        .unwrap();
        let prompt = String::from_utf8(captured.borrow().clone().expect("review step ran"))
            .expect("the prompt is text");
        assert!(prompt.contains("+added line"), "{prompt}");
        assert!(prompt.contains("proj/1/1 approved"), "{prompt}");
        assert!(
            prompt.contains("proj/1/1 changes-requested --reason"),
            "{prompt}"
        );
    }

    #[test]
    fn the_test_step_runs_on_the_diff_the_git_port_reports_since_the_attempt_began() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let captured = RefCell::default();
        let commands = CapturingStep {
            journal: &journal,
            capture_step: TEST_STEP,
            captured: &captured,
        };
        run_queue(
            &journal,
            &clock(),
            &commands,
            &git_with_diff(),
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(60)),
        )
        .unwrap();
        let prompt = String::from_utf8(captured.borrow().clone().expect("test step ran"))
            .expect("the prompt is text");
        assert!(prompt.contains("+added line"), "{prompt}");
        assert!(prompt.contains("proj/1/1 accepted"), "{prompt}");
        assert!(prompt.contains("proj/1/1 rejected --reason"), "{prompt}");
    }

    #[test]
    fn no_report_at_all_ends_the_task_failed_unknown_and_stops_the_run() {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Code(0));
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(report.attempted.len(), 1);
        let attempted = &report.attempted[0];
        assert_eq!(attempted.id, TaskId(1));
        assert_eq!(attempted.status, TaskStatus::FailedUnknown);
        assert!(
            attempted
                .reason
                .as_deref()
                .unwrap()
                .contains("reported nothing"),
            "{:?}",
            attempted.reason
        );
        assert_eq!(
            report.end,
            RunEnd::Stopped {
                id: TaskId(1),
                status: TaskStatus::FailedUnknown,
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::FailedUnknown);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn a_killed_provider_ends_the_task_failed_unknown_and_stops_the_run() {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Killed);
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(report.attempted.len(), 1);
        assert_eq!(report.attempted[0].status, TaskStatus::FailedUnknown);
        assert!(
            report.attempted[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("time limit"),
            "{:?}",
            report.attempted[0].reason
        );
    }

    #[test]
    fn an_interrupted_provider_ends_the_task_failed_unknown_with_the_interrupted_reason_and_exits_the_run()
     {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Interrupted);
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![Attempted {
                    id: TaskId(1),
                    status: TaskStatus::FailedUnknown,
                    reason: Some("the run was interrupted".to_owned()),
                }],
                end: RunEnd::Stopped {
                    id: TaskId(1),
                    status: TaskStatus::FailedUnknown,
                },
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::FailedUnknown);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn a_provider_that_cannot_be_started_ends_the_task_failed_unknown() {
        let journal = journal_of_abc();
        let commands = FakeCommands::returning(Err(CommandsError::new("bash not found")));
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(report.attempted.len(), 1);
        assert_eq!(report.attempted[0].status, TaskStatus::FailedUnknown);
        assert!(
            report.attempted[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("bash not found"),
            "{:?}",
            report.attempted[0].reason
        );
    }

    #[test]
    fn a_provider_that_cannot_build_a_command_ends_the_task_failed_unknown() {
        let journal = journal_of_abc();
        let refusing = Provider {
            name: "refusing",
            command: |_, _| Err("cannot build it".to_owned()),
        };
        let commands = commands_ok(Exit::Code(0));
        let report = run(&journal, &commands, &refusing, Duration::from_secs(60)).unwrap();
        assert_eq!(report.attempted.len(), 1);
        assert_eq!(report.attempted[0].status, TaskStatus::FailedUnknown);
        assert!(
            report.attempted[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("cannot build it"),
            "{:?}",
            report.attempted[0].reason
        );
        // The provider itself is never run: it fails before ever calling `commands.run`.
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn the_provider_runs_in_the_projects_directory_with_the_attempts_timeout() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let commands = commands_ok(Exit::Code(0));
        run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(42)),
        )
        .unwrap();
        let spec = commands.last.borrow().clone().unwrap();
        assert_eq!(spec.dir, Path::new("/work/proj"));
        assert_eq!(spec.timeout, Duration::from_secs(42));
        assert_eq!(spec.args[1], "proj/1/1");
        assert_eq!(spec.args[3], IMPLEMENTATION);
    }

    #[test]
    fn the_implementation_review_test_and_commit_steps_each_begin_and_end_with_one_journal_event() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 4, "{:?}", attempt.steps);
        let implementation = &attempt.steps[0];
        assert_eq!(implementation.name, IMPLEMENTATION);
        let end = implementation.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);
        assert_eq!(end.reported, Some(Outcome::Done));
        let review = &attempt.steps[1];
        assert_eq!(review.name, REVIEW_STEP);
        let end = review.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);
        assert_eq!(end.reported, Some(Outcome::Approved));
        let test = &attempt.steps[2];
        assert_eq!(test.name, TEST_STEP);
        let end = test.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);
        assert_eq!(end.reported, Some(Outcome::Accepted));
        let commit = &attempt.steps[3];
        assert_eq!(commit.name, COMMIT_STEP);
        let end = commit.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);

        let started = journal
            .events()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::StepStarted { .. }))
            .count();
        let ended = journal
            .events()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::StepEnded { .. }))
            .count();
        assert_eq!((started, ended), (4, 4));
    }

    #[test]
    fn a_journal_failure_is_passed_on() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        let commands = commands_ok(Exit::Code(0));
        let error = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), failure.to_string());
    }

    #[test]
    fn a_lock_already_held_stops_the_run_before_touching_the_journal_or_running_anything() {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Code(0));
        let lock = FakeRunLock::held_by(Some(4_321));
        let error = run_queue(
            &journal,
            &clock(),
            &commands,
            &FakeGit::default(),
            &test_provider(),
            &lock,
            context(Duration::from_secs(60)),
        )
        .unwrap_err();
        assert_eq!(
            error,
            RunError::Locked(RunLockError::InProgress(Some(4_321)))
        );
        assert!(error.to_string().contains("4321"), "{error}");
        assert!(commands.last.borrow().is_none());
        for task in crate::list_all_tasks(&journal).unwrap() {
            assert_eq!(task.status, TaskStatus::Pending, "{}", task.id);
        }
    }

    #[test]
    fn a_task_left_running_by_a_killed_run_is_marked_failed_unknown_and_stops_the_run() {
        let journal = journal_of_abc();
        // Stands in for a previous run: it started an attempt at task `a` and never ended it,
        // as a kill mid-attempt would leave things.
        crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
        let commands = commands_ok(Exit::Code(0));

        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            report,
            RunReport {
                attempted: vec![Attempted {
                    id: TaskId(1),
                    status: TaskStatus::FailedUnknown,
                    reason: Some("the run was interrupted".to_owned()),
                }],
                end: RunEnd::Stopped {
                    id: TaskId(1),
                    status: TaskStatus::FailedUnknown,
                },
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::FailedUnknown);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
        assert_eq!(tasks[2].status, TaskStatus::Pending);
        // The provider is never run for the interrupted task: it already ran, unsupervised,
        // in the run that was killed.
        assert!(commands.last.borrow().is_none());
    }

    /// Ends task `id`'s one attempt at `status` with `reason`, as a previous run would have
    /// left it.
    fn end_task_from_a_previous_run(
        journal: &FakeJournal,
        id: TaskId,
        status: TaskStatus,
        reason: &'static str,
    ) {
        let number = crate::attempt::begin_attempt(journal, &clock(), id).unwrap();
        crate::attempt::end_attempt(
            journal,
            id,
            number,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(1),
                status,
                reason: Some(reason),
            },
            clock().0,
        )
        .unwrap();
    }

    #[test]
    fn a_task_already_failed_stops_the_run_before_attempting_anything() {
        let journal = journal_of_abc();
        end_task_from_a_previous_run(&journal, TaskId(1), TaskStatus::Failed, "it broke");
        let commands = commands_ok(Exit::Code(0));

        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::Blocked {
                    id: TaskId(1),
                    status: TaskStatus::Failed,
                    reason: Some("it broke".to_owned()),
                },
            }
        );
        let tasks = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Failed);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
        assert_eq!(tasks[2].status, TaskStatus::Pending);
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_task_already_blocked_stops_the_run_before_attempting_anything() {
        let journal = journal_of_abc();
        end_task_from_a_previous_run(&journal, TaskId(1), TaskStatus::Blocked, "which path?");
        let commands = commands_ok(Exit::Code(0));

        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            report.end,
            RunEnd::Blocked {
                id: TaskId(1),
                status: TaskStatus::Blocked,
                reason: Some("which path?".to_owned()),
            }
        );
        assert!(report.attempted.is_empty());
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_task_already_failed_unknown_stops_the_run_before_attempting_anything() {
        let journal = journal_of_abc();
        end_task_from_a_previous_run(&journal, TaskId(1), TaskStatus::FailedUnknown, "crashed");
        let commands = commands_ok(Exit::Code(0));

        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            report.end,
            RunEnd::Blocked {
                id: TaskId(1),
                status: TaskStatus::FailedUnknown,
                reason: Some("crashed".to_owned()),
            }
        );
        assert!(report.attempted.is_empty());
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_failed_task_past_an_earlier_done_one_still_stops_the_run_before_attempting_anything() {
        let journal = journal_of_abc();
        let number = crate::attempt::begin_attempt(&journal, &clock(), TaskId(1)).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            number,
            AttemptRun {
                duration: Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
            },
            clock().0,
        )
        .unwrap();
        end_task_from_a_previous_run(&journal, TaskId(2), TaskStatus::Failed, "it broke");
        let commands = commands_ok(Exit::Code(0));

        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            report.end,
            RunEnd::Blocked {
                id: TaskId(2),
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
            }
        );
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn removing_the_blocking_task_lets_the_next_run_continue_with_the_task_after_it() {
        let journal = journal_of_abc();
        end_task_from_a_previous_run(&journal, TaskId(1), TaskStatus::Failed, "it broke");
        crate::remove_task(&journal, &clock(), TaskId(1)).unwrap();

        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            report.attempted,
            vec![
                Attempted {
                    id: TaskId(2),
                    status: TaskStatus::Done,
                    reason: None,
                },
                Attempted {
                    id: TaskId(3),
                    status: TaskStatus::Done,
                    reason: None,
                },
            ]
        );
        assert_eq!(report.end, RunEnd::Completed);
    }

    #[test]
    fn a_push_confirmed_on_the_remote_ends_the_task_done_with_its_own_line() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(Some("abcdef1".to_owned()))),
            push: Some(Ok("abcdef1".to_owned())),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Done
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let push = attempt.steps.last().expect("the push step ran");
        assert_eq!(push.name, PUSH_STEP);
        let end = push.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);
        assert_eq!(end.reason.as_deref(), Some("pushed abcdef1 to origin/main"));
        assert!(*git.push_calls.borrow() >= 1);
    }

    #[test]
    fn a_dirty_tree_with_a_configured_identity_is_committed_and_the_step_shows_its_short_hash() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(Some("abc1234".to_owned()))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Done
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let commit = attempt.steps.last().expect("the commit step ran");
        assert_eq!(commit.name, COMMIT_STEP);
        let end = commit.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);
        assert_eq!(end.reason.as_deref(), Some("committed as abc1234"));
        // The attempt's own reason — as opposed to the commit step's own line — is untouched:
        // it means why the attempt is not `done`, and it is.
        assert_eq!(report.attempted[0].reason, None);
        assert!(*git.commit_all_calls.borrow() >= 1);
    }

    #[test]
    fn a_clean_tree_makes_no_commit_and_the_step_says_so() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(None)),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Done
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let commit = attempt.steps.last().expect("the commit step ran");
        assert_eq!(commit.name, COMMIT_STEP);
        let end = commit.ended.as_ref().expect("the step ended");
        assert_eq!(end.status, TaskStatus::Done);
        assert_eq!(end.reason.as_deref(), Some("nothing was changed"));
    }

    #[test]
    fn an_unconfigured_git_identity_refuses_the_commit_and_ends_the_task_failed() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Err(CommitAllError::IdentityNotConfigured)),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(
            report.end,
            RunEnd::Stopped {
                id: TaskId(1),
                status: TaskStatus::Failed,
            }
        );
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Failed
        );
        let reason = report.attempted[0].reason.as_deref().unwrap();
        assert!(
            reason.contains("git identity is not configured"),
            "{reason}"
        );
        assert!(reason.contains("user.name"), "{reason}");
        assert!(reason.contains("user.email"), "{reason}");
    }

    #[test]
    fn a_commit_the_git_port_refuses_ends_the_task_failed_with_its_reason() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Err(CommitAllError::Failed(
                "`git commit` exited with code 1: the pre-commit hook refused it".to_owned(),
            ))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(
            report.end,
            RunEnd::Stopped {
                id: TaskId(1),
                status: TaskStatus::Failed,
            }
        );
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Failed
        );
        let reason = report.attempted[0].reason.as_deref().unwrap();
        assert!(reason.contains("git commit"), "{reason}");
        assert!(reason.contains("exited with code 1"), "{reason}");
        assert!(
            reason.contains("the pre-commit hook refused it"),
            "{reason}"
        );
    }

    #[test]
    fn a_push_rejected_because_the_remote_moved_on_ends_the_task_failed_and_says_so() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(Some("abc1234".to_owned()))),
            push: Some(Err(crate::PushError::Rejected)),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(
            report.end,
            RunEnd::Stopped {
                id: TaskId(1),
                status: TaskStatus::Failed,
            }
        );
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Failed
        );
        let reason = report.attempted[0].reason.as_deref().unwrap();
        assert!(reason.contains("has moved on"), "{reason}");
        assert!(reason.contains("run again"), "{reason}");
    }

    #[test]
    fn a_push_the_git_port_refuses_ends_the_task_failed_with_its_reason() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(Some("abc1234".to_owned()))),
            push: Some(Err(crate::PushError::Failed(
                "`git push origin HEAD:refs/heads/main` exited with code 128: fatal: could not \
                 read from remote repository"
                    .to_owned(),
            ))),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(
            report.end,
            RunEnd::Stopped {
                id: TaskId(1),
                status: TaskStatus::Failed,
            }
        );
        let reason = report.attempted[0].reason.as_deref().unwrap();
        assert!(reason.contains("git push"), "{reason}");
        assert!(reason.contains("exited with code 128"), "{reason}");
        assert!(reason.contains("could not read from remote"), "{reason}");
    }

    #[test]
    fn no_commit_made_leaves_no_push_line_even_with_a_tracked_branch() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(None)),
            ..FakeGit::default()
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            context_tracking(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Done
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert!(
            !attempt.steps.iter().any(|step| step.name == PUSH_STEP),
            "{:?}",
            attempt.steps
        );
        assert_eq!(*git.push_calls.borrow(), 0);
    }

    #[test]
    fn switching_commit_off_skips_it_and_leaves_no_line_even_though_there_is_something_to_commit() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(Some("abc1234".to_owned()))),
            ..FakeGit::default()
        };
        let mut ctx = context(Duration::from_secs(60));
        ctx.disabled_steps = &[COMMIT_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Done
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let names: Vec<_> = attempt
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect();
        assert_eq!(names, [IMPLEMENTATION, REVIEW_STEP, TEST_STEP]);
        assert_eq!(*git.commit_all_calls.borrow(), 0);
    }

    #[test]
    fn switching_push_off_skips_it_even_with_a_tracked_branch_and_a_commit_made() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let git = FakeGit {
            commit_all: Some(Ok(Some("abc1234".to_owned()))),
            ..FakeGit::default()
        };
        // Sync is switched off so it never touches the commit step's own git state.
        let mut ctx = context_tracking(Duration::from_secs(60));
        ctx.disabled_steps = &[SYNC_STEP, PUSH_STEP];
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
            &git,
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        assert_eq!(
            crate::list_all_tasks(&journal).unwrap()[0].status,
            TaskStatus::Done
        );
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        let commit = attempt.steps.last().expect("the commit step ran");
        assert_eq!(commit.name, COMMIT_STEP);
        assert!(
            !attempt.steps.iter().any(|step| step.name == PUSH_STEP),
            "{:?}",
            attempt.steps
        );
        assert_eq!(*git.push_calls.borrow(), 0);
    }
}

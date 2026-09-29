//! `run`: takes the pending tasks in queue order and attempts each one, once, with the
//! [`Provider`] it is given.

use std::error::Error;
use std::fmt::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use crate::{
    AttemptRun, AttemptToken, BeginAttemptError, Clock, CommandSpec, Commands, CommandsError, Exit,
    HEALTH_CHECK_STEP, IMPLEMENTATION, Journal, JournalError, Outcome, Output, Provider,
    ProviderRunError, RecordReportError, RunLock, RunLockError, StepCall, Task, TaskId, TaskKind,
    TaskStatus, list_tasks, run_provider,
};

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

/// The reason recorded for the task a killed run left running, found still running when the
/// next run starts.
const INTERRUPTED: &str = "the run was interrupted";

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
}

/// The prompt for attempt `token` of `task`: its title, body and acceptance criteria, and
/// the exact `report` command, run through `binary_path`, to run for each possible outcome.
/// The full path is used, rather than the name `ktask-rs`, so the command works whether or
/// not the binary that is running is on the agent's `PATH`.
#[must_use]
pub fn build_prompt(task: &Task, token: &AttemptToken, binary_path: &Path) -> String {
    let mut prompt = format!("# {}\n", task.title);
    if !task.body.is_empty() {
        prompt.push('\n');
        prompt.push_str(&task.body);
        prompt.push('\n');
    }
    prompt.push_str("\n## Acceptance criteria\n\n");
    for criterion in &task.criteria {
        prompt.push_str("- ");
        prompt.push_str(criterion);
        prompt.push('\n');
    }
    let binary = binary_path.display();
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         When you are done, run exactly one of these, with the outcome that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} done\n\
         \x20\x20\x20\x20{binary} report --token {token} failed --reason \"<why>\"\n\
         \x20\x20\x20\x20{binary} report --token {token} needs-input --reason \"<why>\"\n\
         \x20\x20\x20\x20{binary} report --token {token} too-large --reason \"<why>\"\n"
    );
    prompt
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
        AttemptRun {
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

/// What [`pick_next_task`] found the run should do next.
enum Pick {
    /// Attempt this pending task.
    Task(Task),
    /// Stop: the next pending task is kind `human`.
    Human(TaskId),
    /// Stop: the first task in queue order that is not `done` already ended `failed`,
    /// `blocked` or `failed-unknown` — the run refuses to skip past it.
    Blocked {
        /// The task the run refuses to skip past.
        id: TaskId,
        /// What it ended at.
        status: TaskStatus,
        /// Why, from its last attempt.
        reason: Option<String>,
    },
    /// Stop: nothing is pending — `queue_is_empty` says whether the queue holds no tasks at
    /// all, or holds tasks that are all already decided.
    NothingLeft { queue_is_empty: bool },
}

/// Looks at the queue in order and decides what the run does next: the first task not
/// already `done`. A `pending` task is attempted (or stops the run, when it is kind
/// `human`); a task that already ended `failed`, `blocked` or `failed-unknown` stops the run
/// without attempting anything, since the queue runs in order and nothing after it may run
/// ahead of it.
///
/// # Errors
///
/// Fails when the journal cannot be read.
fn pick_next_task(journal: &impl Journal) -> Result<Pick, RunError> {
    let tasks = list_tasks(journal)?;
    let Some(next) = tasks.iter().find(|task| task.status != TaskStatus::Done) else {
        return Ok(Pick::NothingLeft {
            queue_is_empty: tasks.is_empty(),
        });
    };
    match next.status {
        TaskStatus::Pending => Ok(if next.kind == TaskKind::Human {
            Pick::Human(next.id)
        } else {
            Pick::Task(next.clone())
        }),
        TaskStatus::Failed | TaskStatus::Blocked | TaskStatus::FailedUnknown => {
            let reason = crate::attempt::last_attempt(journal, next.id)?
                .and_then(|attempt| attempt.ended)
                .and_then(|ended| ended.reason);
            Ok(Pick::Blocked {
                id: next.id,
                status: next.status,
                reason,
            })
        }
        TaskStatus::Running | TaskStatus::Cancelled | TaskStatus::Done => unreachable!(
            "a task left running is resolved before this loop runs; cancelled and done are filtered out above"
        ),
    }
}

/// Why the run ends when nothing is left pending: `Completed` when this run attempted
/// something first, otherwise `EmptyQueue` or `NothingPending` depending on `queue_is_empty`.
fn end_when_nothing_left(attempted: &[Attempted], queue_is_empty: bool) -> RunEnd {
    if !attempted.is_empty() {
        RunEnd::Completed
    } else if queue_is_empty {
        RunEnd::EmptyQueue
    } else {
        RunEnd::NothingPending
    }
}

/// The steps every task's attempt runs through, in order, stopping at the first that ends
/// badly: for now, just the one that always existed — running the provider on the task's whole
/// prompt. Later tasks each add one more name here, and one more way of running it. The
/// health-check step, when the project has configured one, runs ahead of an attempt even
/// being begun, so it is not one of these.
const STEPS: &[&str] = &[IMPLEMENTATION];

/// How many lines of a failing health check's combined output are shown to the operator.
const HEALTH_CHECK_OUTPUT_TAIL_LINES: usize = 20;

/// What running the project's health check produced.
enum HealthCheck {
    /// It exited zero: `duration` is how long it took.
    Passed(Duration),
    /// It did not: `reason` says how, `output_tail` is the end of what it printed.
    Failed { reason: String, output_tail: String },
}

/// The last [`HEALTH_CHECK_OUTPUT_TAIL_LINES`] lines of `output`'s combined standard output
/// and standard error.
fn output_tail(output: &Output) -> String {
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&combined);
    let lines: Vec<&str> = text.lines().collect();
    let tail: Vec<&str> = lines
        .iter()
        .rev()
        .take(HEALTH_CHECK_OUTPUT_TAIL_LINES)
        .rev()
        .copied()
        .collect();
    tail.join("\n")
}

/// Runs `command` with `bash -c` in `context.project_dir`, subject to `context.attempt_timeout`
/// the same way an attempt's own steps are.
fn run_health_check(
    commands: &impl Commands,
    clock: &impl Clock,
    command: &str,
    context: RunContext<'_>,
) -> HealthCheck {
    let started = clock.now();
    let spec = CommandSpec {
        program: "bash".to_owned(),
        args: vec!["-c".to_owned(), command.to_owned()],
        dir: context.project_dir.to_owned(),
        stdin: Vec::new(),
        timeout: context.attempt_timeout,
    };
    let result = commands.run(&spec);
    let duration = clock.now().duration_since(started).unwrap_or_default();
    health_check_outcome(result, duration)
}

/// What `run_health_check` found, turned into a [`HealthCheck`].
fn health_check_outcome(result: Result<Output, CommandsError>, duration: Duration) -> HealthCheck {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return HealthCheck::Failed {
                reason: format!("the health check could not be run: {error}"),
                output_tail: String::new(),
            };
        }
    };
    match output.exit {
        Exit::Code(0) => HealthCheck::Passed(duration),
        Exit::Code(code) => HealthCheck::Failed {
            reason: format!("the health check exited with code {code}"),
            output_tail: output_tail(&output),
        },
        Exit::Killed => HealthCheck::Failed {
            reason: "the health check ran past its time limit and was killed".to_owned(),
            output_tail: output_tail(&output),
        },
        Exit::Interrupted => HealthCheck::Failed {
            reason: INTERRUPTED.to_owned(),
            output_tail: output_tail(&output),
        },
    }
}

/// Records the health check that already ran, in `duration`, as the first step of attempt
/// `number` of task `id` — begun and ended in the same call, since it ran before the attempt
/// itself was begun.
fn record_health_check_step(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    number: u32,
    duration: Duration,
) -> Result<(), RunError> {
    crate::attempt::begin_step(journal, clock, id, number, HEALTH_CHECK_STEP)?;
    crate::attempt::end_step(
        journal,
        clock,
        id,
        number,
        HEALTH_CHECK_STEP,
        AttemptRun {
            duration,
            exit_code: Some(0),
            status: TaskStatus::Done,
            reason: None,
        },
    )?;
    Ok(())
}

/// Runs `provider` on `target.prompt` for `target.token`'s step `step`, timing it from `clock`.
fn timed_provider_run(
    commands: &impl Commands,
    provider: &Provider,
    clock: &impl Clock,
    target: &RunTarget<'_>,
    step: &str,
    context: RunContext<'_>,
) -> (Duration, Result<Output, ProviderRunError>) {
    let started = clock.now();
    let result = run_provider(
        commands,
        provider,
        target.prompt,
        StepCall {
            token: &target.token.to_string(),
            attempt: target.token.number,
            step,
        },
        context.project_dir,
        context.attempt_timeout,
    );
    (
        clock.now().duration_since(started).unwrap_or_default(),
        result,
    )
}

/// The task an attempt runs, the token identifying that attempt, and the whole prompt built
/// for it — everything [`run_steps`] needs about what it is running, as opposed to how.
struct RunTarget<'a> {
    task: &'a Task,
    token: &'a AttemptToken,
    prompt: &'a str,
}

/// Runs `target`'s attempt through `steps`, in order: begins each step, runs the provider on
/// its prompt for it, decides its outcome, and ends it with that outcome — stopping at the
/// first step that does not end `done`, so a step after it is never begun and leaves no event
/// in the journal. Returns the steps' combined duration and the outcome of the last one run,
/// which becomes the whole attempt's.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn run_steps(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    provider: &Provider,
    context: RunContext<'_>,
    target: &RunTarget<'_>,
    steps: &[&str],
) -> Result<(Duration, Option<i32>, TaskStatus, Option<String>), RunError> {
    let mut total = Duration::ZERO;
    let mut exit_code = None;
    let mut status = TaskStatus::Done;
    let mut reason = None;
    for &step in steps {
        crate::attempt::begin_step(journal, clock, target.task.id, target.token.number, step)?;
        let (duration, result) =
            timed_provider_run(commands, provider, clock, target, step, context);
        (exit_code, status, reason) = attempt_outcome(journal, target.task, target.token, result)?;
        total += duration;
        crate::attempt::end_step(
            journal,
            clock,
            target.task.id,
            target.token.number,
            step,
            AttemptRun {
                duration,
                exit_code,
                status,
                reason: reason.as_deref(),
            },
        )?;
        if status != TaskStatus::Done {
            break;
        }
    }
    Ok((total, exit_code, status, reason))
}

/// Runs one attempt at `task` with `provider`: begins it, records `health_check_duration` as
/// the attempt's first step when the health check ran ahead of it, builds the prompt, runs it
/// through the pipeline's steps, and ends the attempt with the outcome they left it at.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn run_one_attempt(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    provider: &Provider,
    context: RunContext<'_>,
    task: &Task,
    health_check_duration: Option<Duration>,
) -> Result<Attempted, RunError> {
    let number = crate::attempt::begin_attempt_running(journal, clock, task.id, provider.name)?;
    if let Some(duration) = health_check_duration {
        record_health_check_step(journal, clock, task.id, number, duration)?;
    }
    let token = AttemptToken::new(context.project_name, task.id, number);
    let prompt = build_prompt(task, &token, context.binary_path);
    let target = RunTarget {
        task,
        token: &token,
        prompt: &prompt,
    };

    let (duration, exit_code, status, reason) =
        run_steps(journal, clock, commands, provider, context, &target, STEPS)?;

    crate::attempt::end_attempt(
        journal,
        task.id,
        token.number,
        AttemptRun {
            duration: health_check_duration.unwrap_or_default() + duration,
            exit_code,
            status,
            reason: reason.as_deref(),
        },
        clock.now(),
    )?;
    Ok(Attempted {
        id: task.id,
        status,
        reason,
    })
}

/// Picks and attempts pending tasks, one at a time, until the queue stops the run: a task of
/// kind `human`, an attempt that does not report `done`, an earlier task already left
/// `failed`, `blocked` or `failed-unknown`, or nothing left pending.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn attempt_loop(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    provider: &Provider,
    context: RunContext<'_>,
) -> Result<RunReport, RunError> {
    let mut attempted = Vec::new();
    loop {
        match pick_next_task(journal)? {
            Pick::Task(task) => {
                let health_check_duration = match context.health_check_command {
                    Some(command) => match run_health_check(commands, clock, command, context) {
                        HealthCheck::Passed(duration) => Some(duration),
                        HealthCheck::Failed {
                            reason,
                            output_tail,
                        } => {
                            let end = RunEnd::HealthCheckFailed {
                                id: task.id,
                                command: command.to_owned(),
                                reason,
                                output_tail,
                            };
                            return Ok(RunReport { attempted, end });
                        }
                    },
                    None => None,
                };
                let result = run_one_attempt(
                    journal,
                    clock,
                    commands,
                    provider,
                    context,
                    &task,
                    health_check_duration,
                )?;
                let status = result.status;
                attempted.push(result);
                if status != TaskStatus::Done {
                    let end = RunEnd::Stopped {
                        id: task.id,
                        status,
                    };
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
    attempt_loop(journal, clock, commands, provider, context)
}

/// What attempt `token` of `task` ended at, given what running the provider produced: its
/// exit code (`None` when the provider could not be run at all, or was killed), the
/// resulting status, and the reason when it is not `done`.
fn attempt_outcome(
    journal: &impl Journal,
    task: &Task,
    token: &AttemptToken,
    result: Result<Output, ProviderRunError>,
) -> Result<(Option<i32>, TaskStatus, Option<String>), RunError> {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return Ok((
                None,
                TaskStatus::FailedUnknown,
                Some(format!("the provider could not run: {error}")),
            ));
        }
    };
    let exit_code = match output.exit {
        Exit::Code(code) => code,
        Exit::Killed => {
            return Ok((
                None,
                TaskStatus::FailedUnknown,
                Some("the provider ran past its time limit and was killed".to_owned()),
            ));
        }
        Exit::Interrupted => {
            return Ok((
                None,
                TaskStatus::FailedUnknown,
                Some(INTERRUPTED.to_owned()),
            ));
        }
    };
    let report = crate::attempt::last_report(journal, task.id, token.number)?;
    let (status, reason) = match report {
        Some((Outcome::Done, _)) => (TaskStatus::Done, None),
        Some((Outcome::Failed | Outcome::TooLarge, reason)) => (TaskStatus::Failed, reason),
        Some((Outcome::NeedsInput, reason)) => (TaskStatus::Blocked, reason),
        None => (
            TaskStatus::FailedUnknown,
            Some(format!(
                "the provider exited with code {exit_code} and reported nothing"
            )),
        ),
    };
    Ok((Some(exit_code), status, reason))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use crate::fakes::{FakeClock, FakeCommands, FakeJournal, FakeRunLock, at, draft};
    use crate::{
        Event, Exit, Outcome, Placement, ProviderCommand, TaskDraft, TaskKind, TaskStatus,
        add_task, report,
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
            provider,
            &FakeRunLock::free(),
            context(timeout),
        )
    }

    #[test]
    fn build_prompt_carries_the_title_body_criteria_and_the_exact_report_command() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: "Some body text.".to_owned(),
            criteria: vec!["first thing".to_owned(), "second thing".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(7), 3);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let prompt = build_prompt(&task, &token, binary_path);
        assert!(prompt.contains("Do the thing"), "{prompt}");
        assert!(prompt.contains("Some body text."), "{prompt}");
        assert!(prompt.contains("- first thing"), "{prompt}");
        assert!(prompt.contains("- second thing"), "{prompt}");
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 done"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 failed --reason"),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 needs-input --reason"
            ),
            "{prompt}"
        );
        assert!(
            prompt
                .contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 too-large --reason"),
            "{prompt}"
        );
        // No line names the binary by its bare name alone: an agent that runs the shown
        // command must never depend on `ktask-rs` being on its own `PATH`.
        assert!(!prompt.contains("\n    ktask-rs report"), "{prompt}");
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
    /// among its args, then exits with `exit`.
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
            report(
                self.journal,
                &FakeClock(SystemTime::UNIX_EPOCH),
                &token,
                self.outcome,
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
            &test_provider(),
            &FakeRunLock::free(),
            ctx,
        )
        .unwrap();
        assert_eq!(report.end, RunEnd::Completed);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 2, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, HEALTH_CHECK_STEP);
        assert_eq!(
            attempt.steps[0].ended.as_ref().unwrap().status,
            TaskStatus::Done
        );
        assert_eq!(attempt.steps[1].name, IMPLEMENTATION);
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
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
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
    fn every_step_run_begins_and_ends_with_one_journal_event_each() {
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
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        let step = &attempt.steps[0];
        assert_eq!(step.name, IMPLEMENTATION);
        let end = step.ended.as_ref().expect("the step ended");
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
        assert_eq!((started, ended), (1, 1));
    }

    #[test]
    fn a_step_that_ends_badly_stops_the_sequence_and_later_steps_leave_no_event() {
        let journal = journal_of_abc();
        let number =
            crate::attempt::begin_attempt_running(&journal, &clock(), TaskId(1), "test").unwrap();
        let token = AttemptToken::new("proj", TaskId(1), number);
        let task = crate::list_all_tasks(&journal).unwrap().remove(0);
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Failed,
            exit: Exit::Code(0),
        };

        let target = RunTarget {
            task: &task,
            token: &token,
            prompt: "prompt",
        };
        let (_, _, status, reason) = run_steps(
            &journal,
            &clock(),
            &commands,
            &test_provider(),
            context(Duration::from_secs(60)),
            &target,
            &["one", "two"],
        )
        .unwrap();

        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(reason, Some("because".to_owned()));
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        // Only the first, failing step ran: the second is never begun, so it leaves no event
        // and no line of its own.
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, "one");
        assert_eq!(
            attempt.steps[0].ended.as_ref().unwrap().status,
            TaskStatus::Failed
        );
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
}

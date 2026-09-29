//! `run`: takes the pending tasks in queue order and attempts each one, once, with the
//! [`Provider`] it is given.

use std::error::Error;
use std::fmt::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use crate::settings::split_tracked_branch;
use crate::{
    AttemptRun, AttemptToken, BeginAttemptError, COMMIT_STEP, Clock, CommandSpec, Commands,
    CommandsError, Exit, HEALTH_CHECK_STEP, IMPLEMENTATION, Journal, JournalError, Outcome, Output,
    PUSH_STEP, Provider, ProviderRunError, REVIEW_STEP, RecordReportError, RunLock, RunLockError,
    SYNC_STEP, StepCall, TEST_STEP, Task, TaskId, TaskKind, TaskStatus, list_tasks, run_provider,
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

/// The prompt for the review step of attempt `token` of `task`: its title, body and
/// acceptance criteria, the diff the implementation step made — `diff`, empty when there was
/// nothing to compare against or git could not produce one — and the exact `report` command,
/// run through `binary_path`, to run for each possible outcome.
#[must_use]
pub fn build_review_prompt(
    task: &Task,
    token: &AttemptToken,
    binary_path: &Path,
    diff: &str,
) -> String {
    let mut prompt = format!("# Review: {}\n", task.title);
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
    prompt.push_str("\n## What the task changed\n\n```diff\n");
    prompt.push_str(diff);
    if !diff.is_empty() && !diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
    let binary = binary_path.display();
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         Review the diff above against the task and its acceptance criteria. When you are \
         done, run exactly one of these, with the outcome that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} approved\n\
         \x20\x20\x20\x20{binary} report --token {token} changes-requested --reason \"<findings>\"\n"
    );
    prompt
}

/// The prompt for the test step of attempt `token` of `task`: its title, body and acceptance
/// criteria, the diff the implementation step made — `diff`, empty when there was nothing to
/// compare against or git could not produce one — and the exact `report` command, run through
/// `binary_path`, to run for each possible outcome.
#[must_use]
pub fn build_test_prompt(
    task: &Task,
    token: &AttemptToken,
    binary_path: &Path,
    diff: &str,
) -> String {
    let mut prompt = format!("# Test: {}\n", task.title);
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
    prompt.push_str("\n## What the task changed\n\n```diff\n");
    prompt.push_str(diff);
    if !diff.is_empty() && !diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
    let binary = binary_path.display();
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         Try out the change above against the task and its acceptance criteria. When you are \
         done, run exactly one of these, with the outcome that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} accepted\n\
         \x20\x20\x20\x20{binary} report --token {token} rejected --reason \"<what failed>\"\n"
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
/// badly: the provider runs on the task's whole prompt, then again in the reviewer role, then
/// again in the tester role — both on the diff the implementation step made. The health-check
/// step, when the project has configured one, runs ahead of an attempt even being begun; the
/// commit step, and then the push step when the project tracks a branch and the commit step
/// made a commit, run after these, once they have all passed. None of the three spends a
/// token, so none is one of these.
const STEPS: &[&str] = &[IMPLEMENTATION, REVIEW_STEP, TEST_STEP];

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

/// Records a step, named `step`, that already ran and passed, in `duration`, with `reason` —
/// `Some` when it has something to say even though it passed, as the sync step does — as one
/// of the steps of attempt `number` of task `id` ahead of the pipeline's own: begun and ended
/// in the same call, since it ran before the attempt itself was begun.
fn record_passed_step(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    number: u32,
    step: &str,
    duration: Duration,
    reason: Option<&str>,
) -> Result<(), RunError> {
    crate::attempt::begin_step(journal, clock, id, number, step)?;
    crate::attempt::end_step(
        journal,
        clock,
        id,
        number,
        step,
        AttemptRun {
            duration,
            exit_code: Some(0),
            status: TaskStatus::Done,
            reason,
        },
        None,
    )?;
    Ok(())
}

/// Runs `args` as `git`, in `context.project_dir`, subject to `context.attempt_timeout`.
fn run_git(
    commands: &impl Commands,
    context: RunContext<'_>,
    args: &[&str],
) -> Result<Output, CommandsError> {
    commands.run(&CommandSpec {
        program: "git".to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        dir: context.project_dir.to_owned(),
        stdin: Vec::new(),
        timeout: context.attempt_timeout,
    })
}

/// Runs `args` as `git`, turning anything other than a clean exit into an error built by
/// `problem` from a description of what went wrong.
fn require_git<E>(
    commands: &impl Commands,
    context: RunContext<'_>,
    args: &[&str],
    problem: impl Fn(String) -> E,
) -> Result<Output, E> {
    let description = || format!("`git {}`", args.join(" "));
    match run_git(commands, context, args) {
        Ok(output) => match output.exit {
            Exit::Code(0) => Ok(output),
            Exit::Code(code) => Err(problem(format!(
                "{} exited with code {code}: {}",
                description(),
                output_tail(&output)
            ))),
            Exit::Killed => Err(problem(format!(
                "{} ran past its time limit and was killed",
                description()
            ))),
            Exit::Interrupted => Err(problem(INTERRUPTED.to_owned())),
        },
        Err(error) => Err(problem(format!(
            "{} could not be run: {error}",
            description()
        ))),
    }
}

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

/// `git config --get key`'s value in `context`'s project directory, trimmed. `None` when it is
/// unset, blank, or git could not answer — never the identity git might otherwise guess from
/// the machine's own account, which does not count as configured for this step's purposes.
fn git_config_value(
    commands: &impl Commands,
    context: RunContext<'_>,
    key: &str,
) -> Option<String> {
    match run_git(commands, context, &["config", "--get", key]) {
        Ok(output) if matches!(output.exit, Exit::Code(0)) => {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            (!value.is_empty()).then_some(value)
        }
        _ => None,
    }
}

/// Whether `context`'s project directory has both `user.name` and `user.email` configured.
fn git_identity_configured(commands: &impl Commands, context: RunContext<'_>) -> bool {
    git_config_value(commands, context, "user.name").is_some()
        && git_config_value(commands, context, "user.email").is_some()
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
fn commit_everything_changed(
    commands: &impl Commands,
    context: RunContext<'_>,
    task: &Task,
) -> CommitOutcome {
    let dirty = match require_git(commands, context, &["status", "--porcelain"], |s: String| s) {
        Ok(output) => !String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        Err(reason) => return CommitOutcome::Refused(reason),
    };
    if !dirty {
        return CommitOutcome::NothingChanged;
    }
    if !git_identity_configured(commands, context) {
        return CommitOutcome::Refused(IDENTITY_NOT_CONFIGURED.to_owned());
    }
    if let Err(reason) = require_git(commands, context, &["add", "-A"], |s: String| s) {
        return CommitOutcome::Refused(reason);
    }
    let message = build_commit_message(task);
    if let Err(reason) = require_git(
        commands,
        context,
        &["commit", "-m", &message],
        |s: String| s,
    ) {
        return CommitOutcome::Refused(reason);
    }
    match require_git(
        commands,
        context,
        &["rev-parse", "--short", "HEAD"],
        |s: String| s,
    ) {
        Ok(output) => {
            CommitOutcome::Committed(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        Err(reason) => CommitOutcome::Refused(reason),
    }
}

/// Records the commit step as one of attempt `number` of task `id`'s own steps, begun and
/// ended in the same call since — like [`record_passed_step`]'s steps — nothing streams while
/// it runs.
fn record_commit_step(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    number: u32,
    duration: Duration,
    status: TaskStatus,
    reason: Option<&str>,
) -> Result<(), RunError> {
    crate::attempt::begin_step(journal, clock, id, number, COMMIT_STEP)?;
    crate::attempt::end_step(
        journal,
        clock,
        id,
        number,
        COMMIT_STEP,
        AttemptRun {
            duration,
            exit_code: if status == TaskStatus::Done {
                Some(0)
            } else {
                None
            },
            status,
            reason,
        },
        None,
    )?;
    Ok(())
}

/// Runs the commit step for `task`'s attempt `token`, once its test step has passed: times
/// [`commit_everything_changed`], records it as the attempt's next step, and returns how long
/// it took together with what it found and did — whether or not it made a commit, or that it
/// was refused, with why and what is expected of the operator.
///
/// # Errors
///
/// Fails when the journal cannot be written.
fn run_commit_step(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    context: RunContext<'_>,
    task: &Task,
    token: &AttemptToken,
) -> Result<(Duration, CommitOutcome), RunError> {
    let started = clock.now();
    let outcome = commit_everything_changed(commands, context, task);
    let duration = clock.now().duration_since(started).unwrap_or_default();
    let (status, reason) = match &outcome {
        CommitOutcome::NothingChanged => (TaskStatus::Done, Some("nothing was changed".to_owned())),
        CommitOutcome::Committed(hash) => (TaskStatus::Done, Some(format!("committed as {hash}"))),
        CommitOutcome::Refused(why) => (TaskStatus::Failed, Some(why.clone())),
    };
    record_commit_step(
        journal,
        clock,
        task.id,
        token.number,
        duration,
        status,
        reason.as_deref(),
    )?;
    Ok((duration, outcome))
}

/// What pushing the commit step's commit to the project's tracked branch, and confirming the
/// remote holds it, found and did.
enum PushOutcome {
    /// The remote branch's tip is now the commit that was pushed: its short hash.
    Pushed(String),
    /// It could not push, or could not confirm the push landed on the remote: why, and — since
    /// this ends the task `failed` — what is expected of the operator.
    Refused(String),
}

/// Records the push step as one of attempt `number` of task `id`'s own steps, begun and ended
/// in the same call since — like [`record_commit_step`]'s step — nothing streams while it
/// runs.
fn record_push_step(
    journal: &impl Journal,
    clock: &impl Clock,
    id: TaskId,
    number: u32,
    duration: Duration,
    status: TaskStatus,
    reason: Option<&str>,
) -> Result<(), RunError> {
    crate::attempt::begin_step(journal, clock, id, number, PUSH_STEP)?;
    crate::attempt::end_step(
        journal,
        clock,
        id,
        number,
        PUSH_STEP,
        AttemptRun {
            duration,
            exit_code: if status == TaskStatus::Done {
                Some(0)
            } else {
                None
            },
            status,
            reason,
        },
        None,
    )?;
    Ok(())
}

/// Pushes `context`'s project directory's `HEAD` to `tracked_branch` (`"<remote>/<branch>"`),
/// then confirms — with `git ls-remote`, checked live against the remote rather than any
/// locally cached ref — that the branch's tip on the remote is now that commit: a push exiting
/// zero is not itself proof the ref actually moved. Refuses, naming why and what is expected of
/// the operator, when the push itself is rejected or cannot be run, or when the remote's tip
/// does not turn out to match afterwards.
fn push_commit(
    commands: &impl Commands,
    context: RunContext<'_>,
    tracked_branch: &str,
) -> PushOutcome {
    let Some((remote, branch)) = split_tracked_branch(tracked_branch) else {
        unreachable!("a saved tracked-branch setting always names a remote and a branch");
    };
    let local = match require_git(commands, context, &["rev-parse", "HEAD"], |s: String| s) {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        Err(reason) => return PushOutcome::Refused(reason),
    };
    let refspec = format!("HEAD:refs/heads/{branch}");
    let description = format!("`git push {remote} {refspec}`");
    let output = match run_git(commands, context, &["push", remote, &refspec]) {
        Ok(output) => output,
        Err(error) => {
            return PushOutcome::Refused(format!("{description} could not be run: {error}"));
        }
    };
    match output.exit {
        Exit::Code(0) => {}
        Exit::Code(code) => {
            let tail = output_tail(&output);
            let rejected = tail.contains("[rejected]") || tail.contains("non-fast-forward");
            return PushOutcome::Refused(if rejected {
                format!(
                    "{description} was rejected: {tracked_branch} has moved on since this \
                     task's commit was made; bring in the new commits and push it yourself, \
                     then run again"
                )
            } else {
                format!("{description} exited with code {code}: {tail}")
            });
        }
        Exit::Killed => {
            return PushOutcome::Refused(format!(
                "{description} ran past its time limit and was killed"
            ));
        }
        Exit::Interrupted => return PushOutcome::Refused(INTERRUPTED.to_owned()),
    }
    match require_git(
        commands,
        context,
        &["ls-remote", remote, &format!("refs/heads/{branch}")],
        |s: String| s,
    ) {
        Ok(output) => {
            let tip = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .map(str::to_owned);
            match tip {
                Some(tip) if tip == local => {
                    PushOutcome::Pushed(local[..local.len().min(7)].to_owned())
                }
                Some(tip) => PushOutcome::Refused(format!(
                    "{description} exited zero but {tracked_branch}'s tip is now {tip}, not \
                     {local}: confirm manually before running again"
                )),
                None => PushOutcome::Refused(format!(
                    "{description} exited zero but {tracked_branch} could not be found \
                     afterwards: confirm manually before running again"
                )),
            }
        }
        Err(reason) => PushOutcome::Refused(reason),
    }
}

/// Runs the push step for `task_id`'s attempt `token`, once the commit step has made a commit
/// and the project tracks `tracked_branch`: times [`push_commit`], records it as the attempt's
/// next step, and returns how long it took together with what it leaves the attempt at —
/// `done`, with the remote branch confirmed to hold the commit, or `failed` with why and what
/// is expected of the operator.
///
/// # Errors
///
/// Fails when the journal cannot be written.
fn run_push_step(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    context: RunContext<'_>,
    task_id: TaskId,
    token: &AttemptToken,
    tracked_branch: &str,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    let started = clock.now();
    let outcome = push_commit(commands, context, tracked_branch);
    let duration = clock.now().duration_since(started).unwrap_or_default();
    let (status, reason) = match outcome {
        PushOutcome::Pushed(hash) => (
            TaskStatus::Done,
            Some(format!("pushed {hash} to {tracked_branch}")),
        ),
        PushOutcome::Refused(why) => (TaskStatus::Failed, Some(why)),
    };
    record_push_step(
        journal,
        clock,
        task_id,
        token.number,
        duration,
        status,
        reason.as_deref(),
    )?;
    Ok((duration, status, reason))
}

/// The commit `HEAD` names in `context`'s project directory, right now — the baseline the
/// review step's diff is taken against, captured before the implementation step runs so that
/// diff shows only what the task's own attempt changed. `None` when git could not answer.
fn current_commit(commands: &impl Commands, context: RunContext<'_>) -> Option<String> {
    match run_git(commands, context, &["rev-parse", "HEAD"]) {
        Ok(output) if matches!(output.exit, Exit::Code(0)) => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => None,
    }
}

/// Everything changed in `context`'s project directory since `start_commit`, committed or
/// still sitting uncommitted in the working tree — what the review step's prompt shows as the
/// task's own diff. Empty when `start_commit` is `None`, or git could not produce one.
fn diff_since(
    commands: &impl Commands,
    context: RunContext<'_>,
    start_commit: Option<&str>,
) -> String {
    let Some(start_commit) = start_commit else {
        return String::new();
    };
    match run_git(commands, context, &["diff", start_commit]) {
        Ok(output) if matches!(output.exit, Exit::Code(0)) => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
    }
}

/// Every path `git diff --name-only --diff-filter=U` named as conflicting, in `context`'s
/// project directory — read while a rebase there is still stopped on the conflict, before it
/// is undone.
fn conflicted_files(commands: &impl Commands, context: RunContext<'_>) -> Vec<String> {
    require_git(
        commands,
        context,
        &["diff", "--name-only", "--diff-filter=U"],
        SyncProblem::GitFailed,
    )
    .map(|output| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// Pulls `tracked_branch` (`"<remote>/<branch>"`) with rebase into `context.project_dir`: is
/// refused when the directory holds uncommitted changes or the remote cannot be reached;
/// rebases onto the tracked branch when it fetched anything new, undoing the rebase and
/// naming every file it conflicted in when it did. Returns a message for `status` to show —
/// how many commits were taken in, or that there were none — once it succeeded.
fn sync_with_tracked_branch(
    commands: &impl Commands,
    tracked_branch: &str,
    context: RunContext<'_>,
) -> Result<String, SyncProblem> {
    let status_output = require_git(
        commands,
        context,
        &["status", "--porcelain"],
        SyncProblem::GitFailed,
    )?;
    let dirty = String::from_utf8_lossy(&status_output.stdout)
        .trim()
        .to_owned();
    if !dirty.is_empty() {
        return Err(SyncProblem::UncommittedChanges(dirty));
    }

    let Some((remote, branch)) = split_tracked_branch(tracked_branch) else {
        unreachable!("a saved tracked-branch setting always names a remote and a branch");
    };
    require_git(
        commands,
        context,
        &["fetch", remote],
        SyncProblem::RemoteUnreachable,
    )?;

    let remote_ref = format!("{remote}/{branch}");
    let count_output = require_git(
        commands,
        context,
        &["rev-list", "--count", &format!("HEAD..{remote_ref}")],
        SyncProblem::GitFailed,
    )?;
    let count: u64 = String::from_utf8_lossy(&count_output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if count == 0 {
        return Ok("nothing new".to_owned());
    }

    match run_git(commands, context, &["rebase", &remote_ref]) {
        Ok(output) if matches!(output.exit, Exit::Code(0)) => Ok(format!(
            "took in {count} commit{} from {tracked_branch}",
            if count == 1 { "" } else { "s" }
        )),
        Ok(_) => {
            let files = conflicted_files(commands, context);
            let _ = run_git(commands, context, &["rebase", "--abort"]);
            Err(SyncProblem::Conflict(files))
        }
        Err(error) => Err(SyncProblem::GitFailed(format!(
            "`git rebase {remote_ref}` could not be run: {error}"
        ))),
    }
}

/// What the sync ahead of a task's health check produced.
enum Sync {
    /// It found and, when there was anything to bring in, rebased in `message`'s worth of
    /// commits, taking `duration`.
    Passed { duration: Duration, message: String },
    /// It refused to run.
    Failed(SyncProblem),
}

/// Times [`sync_with_tracked_branch`] with `clock`.
fn run_sync(
    commands: &impl Commands,
    clock: &impl Clock,
    tracked_branch: &str,
    context: RunContext<'_>,
) -> Sync {
    let started = clock.now();
    match sync_with_tracked_branch(commands, tracked_branch, context) {
        Ok(message) => Sync::Passed {
            duration: clock.now().duration_since(started).unwrap_or_default(),
            message,
        },
        Err(problem) => Sync::Failed(problem),
    }
}

/// Runs `provider` on `prompt` for `target.token`'s step `step`, timing it from `clock`.
fn timed_provider_run(
    commands: &impl Commands,
    provider: &Provider,
    clock: &impl Clock,
    target: &RunTarget<'_>,
    step: &str,
    prompt: &str,
    context: RunContext<'_>,
) -> (Duration, Result<Output, ProviderRunError>) {
    let started = clock.now();
    let result = run_provider(
        commands,
        provider,
        prompt,
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

/// The task an attempt runs, the token identifying that attempt, the whole prompt built for
/// its implementation step, and the commit `HEAD` named before that step ran — everything
/// [`run_steps`] needs about what it is running, as opposed to how. `start_commit` is `None`
/// when it could not be captured; the review step's diff is then empty rather than the run
/// failing over it.
struct RunTarget<'a> {
    task: &'a Task,
    token: &'a AttemptToken,
    prompt: &'a str,
    start_commit: Option<&'a str>,
}

/// The prompt step `step` of `target`'s attempt runs the provider on: `target`'s own, built
/// once for the implementation step, or a freshly built reviewer's or tester's prompt — the
/// task and the diff since `target.start_commit` — for the review or test step.
fn prompt_for_step(
    commands: &impl Commands,
    context: RunContext<'_>,
    target: &RunTarget<'_>,
    step: &str,
) -> String {
    if step == REVIEW_STEP {
        let diff = diff_since(commands, context, target.start_commit);
        build_review_prompt(target.task, target.token, context.binary_path, &diff)
    } else if step == TEST_STEP {
        let diff = diff_since(commands, context, target.start_commit);
        build_test_prompt(target.task, target.token, context.binary_path, &diff)
    } else {
        target.prompt.to_owned()
    }
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
        let prompt = prompt_for_step(commands, context, target, step);
        let (duration, result) =
            timed_provider_run(commands, provider, clock, target, step, &prompt, context);
        let outcome = attempt_outcome(journal, target.task, target.token, step, result)?;
        exit_code = outcome.exit_code;
        status = outcome.status;
        reason = outcome.reason;
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
            outcome.reported,
        )?;
        if status != TaskStatus::Done {
            break;
        }
    }
    Ok((total, exit_code, status, reason))
}

/// One step that already ran and passed before the attempt it belongs to was even begun — the
/// sync and the health check, when the project has configured them — recorded as the
/// attempt's own first steps, in the order they ran.
struct PreStep {
    /// The step's name: [`SYNC_STEP`] or [`HEALTH_CHECK_STEP`].
    name: &'static str,
    /// How long it took.
    duration: Duration,
    /// What it has to say even though it passed — the sync step's own message, or `None` for
    /// the health check, which has nothing to add.
    reason: Option<String>,
}

/// Runs one attempt at `task` with `provider`: begins it, records `pre_steps` as the attempt's
/// own first steps in order, builds the prompt, runs it through the pipeline's steps, and ends
/// the attempt with the outcome they left it at.
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
    pre_steps: &[PreStep],
) -> Result<Attempted, RunError> {
    let number = crate::attempt::begin_attempt_running(journal, clock, task.id, provider.name)?;
    let mut pre_duration = Duration::ZERO;
    for pre_step in pre_steps {
        record_passed_step(
            journal,
            clock,
            task.id,
            number,
            pre_step.name,
            pre_step.duration,
            pre_step.reason.as_deref(),
        )?;
        pre_duration += pre_step.duration;
    }
    let start_commit = current_commit(commands, context);
    let token = AttemptToken::new(context.project_name, task.id, number);
    let prompt = build_prompt(task, &token, context.binary_path);
    let target = RunTarget {
        task,
        token: &token,
        prompt: &prompt,
        start_commit: start_commit.as_deref(),
    };

    let (steps_duration, exit_code, steps_status, steps_reason) =
        run_steps(journal, clock, commands, provider, context, &target, STEPS)?;

    let (post_duration, status, reason) = if steps_status == TaskStatus::Done {
        let (commit_duration, commit_outcome) =
            run_commit_step(journal, clock, commands, context, task, &token)?;
        match commit_outcome {
            CommitOutcome::Committed(_) => {
                if let Some(tracked_branch) = context.tracked_branch {
                    let (push_duration, push_status, push_reason) = run_push_step(
                        journal,
                        clock,
                        commands,
                        context,
                        task.id,
                        &token,
                        tracked_branch,
                    )?;
                    let duration = commit_duration + push_duration;
                    if push_status == TaskStatus::Done {
                        // Neither the commit nor the push step's own reason — hash, branch,
                        // "nothing to commit" — belongs on the whole attempt's, whose `reason`
                        // means why it is not `done`.
                        (duration, steps_status, steps_reason)
                    } else {
                        (duration, push_status, push_reason)
                    }
                } else {
                    (commit_duration, steps_status, steps_reason)
                }
            }
            CommitOutcome::NothingChanged => (commit_duration, steps_status, steps_reason),
            CommitOutcome::Refused(why) => (commit_duration, TaskStatus::Failed, Some(why)),
        }
    } else {
        (Duration::ZERO, steps_status, steps_reason)
    };

    crate::attempt::end_attempt(
        journal,
        task.id,
        token.number,
        AttemptRun {
            duration: pre_duration + steps_duration + post_duration,
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
                let mut pre_steps = Vec::new();
                if let Some(tracked_branch) = context.tracked_branch {
                    match run_sync(commands, clock, tracked_branch, context) {
                        Sync::Passed { duration, message } => pre_steps.push(PreStep {
                            name: SYNC_STEP,
                            duration,
                            reason: Some(message),
                        }),
                        Sync::Failed(problem) => {
                            let end = RunEnd::SyncFailed {
                                id: task.id,
                                tracked_branch: tracked_branch.to_owned(),
                                problem,
                            };
                            return Ok(RunReport { attempted, end });
                        }
                    }
                }
                if let Some(command) = context.health_check_command {
                    match run_health_check(commands, clock, command, context) {
                        HealthCheck::Passed(duration) => pre_steps.push(PreStep {
                            name: HEALTH_CHECK_STEP,
                            duration,
                            reason: None,
                        }),
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
                    }
                }
                let result = run_one_attempt(
                    journal, clock, commands, provider, context, &task, &pre_steps,
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

/// What a step ended at: its exit code (`None` when the provider could not be run at all, or
/// was killed), the resulting status, the reason when it is not `done`, and the fine-grained
/// outcome the agent itself reported, when it reported anything.
struct StepOutcome {
    exit_code: Option<i32>,
    status: TaskStatus,
    reason: Option<String>,
    reported: Option<Outcome>,
}

impl StepOutcome {
    /// No report could ever have been read for this step: the provider itself never ran to
    /// completion, so there is nothing to distinguish beyond `reason`.
    fn unreported(reason: String) -> Self {
        Self {
            exit_code: None,
            status: TaskStatus::FailedUnknown,
            reason: Some(reason),
            reported: None,
        }
    }
}

/// What attempt `token` of `task`'s step `step` ended at, given what running the provider
/// produced for it.
fn attempt_outcome(
    journal: &impl Journal,
    task: &Task,
    token: &AttemptToken,
    step: &str,
    result: Result<Output, ProviderRunError>,
) -> Result<StepOutcome, RunError> {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return Ok(StepOutcome::unreported(format!(
                "the provider could not run: {error}"
            )));
        }
    };
    let exit_code = match output.exit {
        Exit::Code(code) => code,
        Exit::Killed => {
            return Ok(StepOutcome::unreported(
                "the provider ran past its time limit and was killed".to_owned(),
            ));
        }
        Exit::Interrupted => {
            return Ok(StepOutcome::unreported(INTERRUPTED.to_owned()));
        }
    };
    let report = crate::attempt::report_of_step(journal, task.id, token.number, step)?;
    let reported = report.as_ref().map(|(outcome, _)| *outcome);
    let (status, reason) = match report {
        Some((Outcome::Done | Outcome::Approved | Outcome::Accepted, _)) => {
            (TaskStatus::Done, None)
        }
        Some((
            Outcome::Failed | Outcome::TooLarge | Outcome::ChangesRequested | Outcome::Rejected,
            reason,
        )) => (TaskStatus::Failed, reason),
        Some((Outcome::NeedsInput, reason)) => (TaskStatus::Blocked, reason),
        None => (
            TaskStatus::FailedUnknown,
            Some(format!(
                "the provider exited with code {exit_code} and reported nothing"
            )),
        ),
    };
    Ok(StepOutcome {
        exit_code: Some(exit_code),
        status,
        reason,
        reported,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
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
            tracked_branch: None,
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
    fn build_review_prompt_carries_the_title_criteria_the_diff_and_the_exact_report_commands() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: "Some body text.".to_owned(),
            criteria: vec!["first thing".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(7), 3);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let diff = "--- a/file\n+++ b/file\n+added line\n";
        let prompt = build_review_prompt(&task, &token, binary_path, diff);
        assert!(prompt.contains("Do the thing"), "{prompt}");
        assert!(prompt.contains("Some body text."), "{prompt}");
        assert!(prompt.contains("- first thing"), "{prompt}");
        assert!(prompt.contains("+added line"), "{prompt}");
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 approved"),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 changes-requested --reason"
            ),
            "{prompt}"
        );
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
    /// among its args, then exits with `exit` — except for the review step, which it always
    /// approves, the test step, which it always accepts, and a `git` call, which it answers
    /// with nothing rather than trying to read a token out of git's own arguments: `run` now
    /// runs one of these ahead of every attempt, to capture the commit the review and test
    /// steps' diff is taken against.
    struct ReportingCommands<'a> {
        journal: &'a FakeJournal,
        outcome: Outcome,
        exit: Exit,
    }

    impl Commands for ReportingCommands<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            if spec.program == "git" {
                return Ok(Output {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit: Exit::Code(0),
                });
            }
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
        assert_eq!(attempt.steps.len(), 4, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, IMPLEMENTATION);
        assert_eq!(attempt.steps[1].name, REVIEW_STEP);
        assert_eq!(attempt.steps[2].name, TEST_STEP);
        assert_eq!(attempt.steps[3].name, COMMIT_STEP);
    }

    /// A ready-to-return, always-successful [`Output`] carrying `stdout`.
    fn git_ok(stdout: &[u8]) -> Output {
        Output {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }
    }

    /// An [`Output`] that failed with `code`, saying `stderr`.
    fn git_failed(code: i32, stderr: &[u8]) -> Output {
        Output {
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
            exit: Exit::Code(code),
        }
    }

    /// A commands port that answers `git` invocations by their exact arguments — every other
    /// `git` call it is asked for, not just the ones a test cares about (`rebase --abort`,
    /// say), succeeds with nothing — and passes anything that is not `git` on to `other`.
    /// Records every `git` call it received, in order, so a test can tell what ran.
    struct GitScript<'a> {
        responses: Vec<(&'static [&'static str], Output)>,
        calls: RefCell<Vec<Vec<String>>>,
        other: &'a dyn Commands,
    }

    impl Commands for GitScript<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            if spec.program != "git" {
                return self.other.run(spec);
            }
            self.calls.borrow_mut().push(spec.args.clone());
            Ok(self
                .responses
                .iter()
                .find(|(args, _)| spec.args == *args)
                .map_or_else(|| git_ok(b""), |(_, output)| output.clone()))
        }
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
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = GitScript {
            responses: vec![
                (&["status", "--porcelain"], git_ok(b"")),
                (&["fetch", "origin"], git_ok(b"")),
                (
                    &["rev-list", "--count", "HEAD..origin/main"],
                    git_ok(b"3\n"),
                ),
                (&["rebase", "origin/main"], git_ok(b"")),
            ],
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = GitScript {
            responses: vec![
                (&["status", "--porcelain"], git_ok(b"")),
                (&["fetch", "origin"], git_ok(b"")),
                (
                    &["rev-list", "--count", "HEAD..origin/main"],
                    git_ok(b"1\n"),
                ),
                (&["rebase", "origin/main"], git_ok(b"")),
            ],
            calls: RefCell::default(),
            other: &reporting,
        };
        run_queue(
            &journal,
            &clock(),
            &commands,
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
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = GitScript {
            responses: vec![
                (&["status", "--porcelain"], git_ok(b"")),
                (&["fetch", "origin"], git_ok(b"")),
                (
                    &["rev-list", "--count", "HEAD..origin/main"],
                    git_ok(b"0\n"),
                ),
            ],
            calls: RefCell::default(),
            other: &reporting,
        };
        run_queue(
            &journal,
            &clock(),
            &commands,
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
        assert!(
            !commands
                .calls
                .borrow()
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("rebase")),
            "{:?}",
            commands.calls.borrow()
        );
    }

    #[test]
    fn uncommitted_changes_stop_the_sync_before_anything_else_runs_and_the_task_stays_pending() {
        let journal = journal_of_abc();
        let commands = GitScript {
            responses: vec![(&["status", "--porcelain"], git_ok(b" M file.txt\n"))],
            calls: RefCell::default(),
            other: &NeverRun,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        assert!(
            !commands
                .calls
                .borrow()
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("fetch")),
            "{:?}",
            commands.calls.borrow()
        );
    }

    #[test]
    fn an_unreachable_remote_stops_the_sync_and_the_task_stays_pending() {
        let journal = journal_of_abc();
        let commands = GitScript {
            responses: vec![
                (&["status", "--porcelain"], git_ok(b"")),
                (
                    &["fetch", "origin"],
                    git_failed(128, b"fatal: could not read from remote repository"),
                ),
            ],
            calls: RefCell::default(),
            other: &NeverRun,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        let commands = GitScript {
            responses: vec![
                (&["status", "--porcelain"], git_ok(b"")),
                (&["fetch", "origin"], git_ok(b"")),
                (
                    &["rev-list", "--count", "HEAD..origin/main"],
                    git_ok(b"1\n"),
                ),
                (
                    &["rebase", "origin/main"],
                    git_failed(1, b"CONFLICT (content): Merge conflict in file.txt"),
                ),
                (
                    &["diff", "--name-only", "--diff-filter=U"],
                    git_ok(b"file.txt\nother.txt\n"),
                ),
                (&["rebase", "--abort"], git_ok(b"")),
            ],
            calls: RefCell::default(),
            other: &NeverRun,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        assert!(
            commands
                .calls
                .borrow()
                .iter()
                .any(|args| args.as_slice() == ["rebase", "--abort"]),
            "{:?}",
            commands.calls.borrow()
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
        let commands = GitScript {
            responses: vec![
                (&["status", "--porcelain"], git_ok(b"")),
                (&["fetch", "origin"], git_ok(b"")),
                (
                    &["rev-list", "--count", "HEAD..origin/main"],
                    git_ok(b"0\n"),
                ),
            ],
            calls: RefCell::default(),
            other: &HealthCheckAnd {
                health_check: Ok(Output {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit: Exit::Code(0),
                }),
                other: &reporting,
            },
        };
        let mut ctx = context_tracking(Duration::from_secs(60));
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
            if spec.program == "git" {
                return Ok(Output {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit: Exit::Code(0),
                });
            }
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
            if spec.program == "git" {
                return Ok(Output {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit: Exit::Code(0),
                });
            }
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

    /// A commands port that reports `Outcome::Done` for the implementation step and captures
    /// the prompt (its standard input) it is run with for the review step, in `captured`,
    /// before reporting `Outcome::Approved` for it too. Answers `git rev-parse HEAD` and
    /// `git diff <sha>` as `GitScript` would, from `responses`.
    /// A commands port that reports `Outcome::Done` for the implementation step,
    /// `Outcome::Approved` for the review step and `Outcome::Accepted` for the test step,
    /// capturing the prompt (its standard input) it is run with for whichever step is named
    /// `capture_step`, in `captured`. Answers `git rev-parse HEAD` and `git diff <sha>` as
    /// `GitScript` would, from `responses`.
    struct CapturingStep<'a> {
        journal: &'a FakeJournal,
        capture_step: &'static str,
        responses: Vec<(&'static [&'static str], Output)>,
        captured: &'a RefCell<Option<Vec<u8>>>,
    }

    impl Commands for CapturingStep<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            if spec.program == "git" {
                return Ok(self
                    .responses
                    .iter()
                    .find(|(args, _)| spec.args == *args)
                    .map_or_else(|| git_ok(b""), |(_, output)| output.clone()));
            }
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

    #[test]
    fn the_review_step_runs_on_the_diff_git_reports_since_the_attempt_began() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let captured = RefCell::default();
        let commands = CapturingStep {
            journal: &journal,
            capture_step: REVIEW_STEP,
            responses: vec![
                (&["rev-parse", "HEAD"], git_ok(b"abc123\n")),
                (
                    &["diff", "abc123"],
                    git_ok(b"--- a/file\n+++ b/file\n+added line\n"),
                ),
            ],
            captured: &captured,
        };
        run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
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
    fn the_test_step_runs_on_the_diff_git_reports_since_the_attempt_began() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let captured = RefCell::default();
        let commands = CapturingStep {
            journal: &journal,
            capture_step: TEST_STEP,
            responses: vec![
                (&["rev-parse", "HEAD"], git_ok(b"abc123\n")),
                (
                    &["diff", "abc123"],
                    git_ok(b"--- a/file\n+++ b/file\n+added line\n"),
                ),
            ],
            captured: &captured,
        };
        run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
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
        // The provider itself is never run: the one command that did run is `run`'s own git
        // call, capturing the commit the review step's diff would be taken against.
        assert_eq!(
            commands.last.borrow().as_ref().map(|spec| &spec.program),
            Some(&"git".to_owned())
        );
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
            start_commit: None,
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

    /// A commands port that answers every `git` call the commit step could make, driven by
    /// `dirty` (whether `status --porcelain` reports changes), `identity` (whether
    /// `config --get` finds `user.name` and `user.email`) and `commit_exit` (what `commit`
    /// itself exits with) — and passes anything that is not `git` on to `other`. Records every
    /// `git` call it received, in order.
    struct CommitScript<'a> {
        dirty: bool,
        identity: bool,
        commit_exit: Exit,
        hash: &'static str,
        calls: RefCell<Vec<Vec<String>>>,
        other: &'a dyn Commands,
    }

    impl Commands for CommitScript<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            if spec.program != "git" {
                return self.other.run(spec);
            }
            self.calls.borrow_mut().push(spec.args.clone());
            match spec.args.first().map(String::as_str) {
                Some("status") => Ok(git_ok(if self.dirty { b"M file.txt\n" } else { b"" })),
                Some("config") => {
                    if self.identity {
                        Ok(git_ok(b"configured\n"))
                    } else {
                        Ok(Output {
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                            exit: Exit::Code(1),
                        })
                    }
                }
                Some("commit") => Ok(Output {
                    stdout: Vec::new(),
                    stderr: b"the pre-commit hook refused it\n".to_vec(),
                    exit: self.commit_exit,
                }),
                Some("rev-parse") => Ok(git_ok(self.hash.as_bytes())),
                _ => Ok(git_ok(b"")),
            }
        }
    }

    #[test]
    fn a_dirty_tree_with_a_configured_identity_is_committed_and_the_step_shows_its_short_hash() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = CommitScript {
            dirty: true,
            identity: true,
            commit_exit: Exit::Code(0),
            hash: "abc1234",
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
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
        let calls = commands.calls.borrow();
        assert!(
            calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("add")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("commit")),
            "{calls:?}"
        );
    }

    #[test]
    fn a_clean_tree_makes_no_commit_and_the_step_says_so() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = CommitScript {
            dirty: false,
            identity: true,
            commit_exit: Exit::Code(0),
            hash: "abc1234",
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
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
        let calls = commands.calls.borrow();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("add")),
            "no staging should happen when nothing changed: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("commit")),
            "no commit should happen when nothing changed: {calls:?}"
        );
    }

    #[test]
    fn an_unconfigured_git_identity_refuses_the_commit_and_ends_the_task_failed() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = CommitScript {
            dirty: true,
            identity: false,
            commit_exit: Exit::Code(0),
            hash: "abc1234",
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
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
        let calls = commands.calls.borrow();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("add")),
            "nothing should be staged once the identity check refuses: {calls:?}"
        );
    }

    #[test]
    fn a_commit_git_itself_refuses_ends_the_task_failed_with_what_git_said() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = CommitScript {
            dirty: true,
            identity: true,
            commit_exit: Exit::Code(1),
            hash: "abc1234",
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run(
            &journal,
            &commands,
            &test_provider(),
            Duration::from_secs(60),
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

    /// A commands port that answers every `git` call the sync, commit and push steps could
    /// make: a tracked branch with nothing new (so the sync passes at once), a dirty tree, a
    /// configured identity and a clean commit (so the commit step always succeeds) — then
    /// `push_exit`, `push_stdout` and `push_stderr` for the push itself, and `remote_tip` for
    /// what `git ls-remote` reports afterwards. `status --porcelain` runs once for the sync
    /// step, ahead of the attempt, and once for the commit step, inside it: the first call
    /// must answer clean or the sync itself would refuse to run, so only calls after the first
    /// are shown dirty. Records every `git` call it received, in order.
    struct PushScript<'a> {
        local_hash: &'static str,
        push_exit: Exit,
        push_stdout: &'static [u8],
        push_stderr: &'static [u8],
        remote_tip: Option<&'static str>,
        status_calls: RefCell<u32>,
        calls: RefCell<Vec<Vec<String>>>,
        other: &'a dyn Commands,
    }

    impl Commands for PushScript<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
            if spec.program != "git" {
                return self.other.run(spec);
            }
            self.calls.borrow_mut().push(spec.args.clone());
            match spec.args.first().map(String::as_str) {
                Some("status") => {
                    let mut count = self.status_calls.borrow_mut();
                    *count += 1;
                    // Each task's attempt makes exactly two `status --porcelain` calls: the
                    // sync step's, ahead of it, then the commit step's, inside it — so the
                    // odd ones (sync) must answer clean, or the sync itself would refuse to
                    // run, and the even ones (commit) are shown dirty.
                    Ok(git_ok(if *count % 2 == 1 {
                        b""
                    } else {
                        b"M file.txt\n"
                    }))
                }
                Some("rev-list") => Ok(git_ok(b"0\n")),
                Some("config") => Ok(git_ok(b"configured\n")),
                Some("rev-parse") => Ok(git_ok(self.local_hash.as_bytes())),
                Some("push") => Ok(Output {
                    stdout: self.push_stdout.to_vec(),
                    stderr: self.push_stderr.to_vec(),
                    exit: self.push_exit,
                }),
                Some("ls-remote") => Ok(git_ok(
                    self.remote_tip
                        .map(|tip| format!("{tip}\trefs/heads/main\n"))
                        .unwrap_or_default()
                        .as_bytes(),
                )),
                _ => Ok(git_ok(b"")),
            }
        }
    }

    #[test]
    fn a_push_confirmed_on_the_remote_ends_the_task_done_with_its_own_line() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let hash = "abcdef1234567890abcdef1234567890abcdef12";
        let commands = PushScript {
            local_hash: hash,
            push_exit: Exit::Code(0),
            push_stdout: b"",
            push_stderr: b"",
            remote_tip: Some(hash),
            status_calls: RefCell::default(),
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        let calls = commands.calls.borrow();
        assert!(
            calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("push")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("ls-remote")),
            "{calls:?}"
        );
    }

    #[test]
    fn a_push_rejected_because_the_remote_moved_on_ends_the_task_failed_and_says_so() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = PushScript {
            local_hash: "abcdef1234567890abcdef1234567890abcdef12",
            push_exit: Exit::Code(1),
            push_stdout: b"",
            push_stderr: b" ! [rejected]        HEAD -> main (fetch first)\n",
            remote_tip: None,
            status_calls: RefCell::default(),
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        assert!(
            !commands
                .calls
                .borrow()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("ls-remote")),
            "a rejected push is never confirmed against the remote"
        );
    }

    #[test]
    fn a_push_that_cannot_reach_the_remote_ends_the_task_failed_with_what_git_said() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = PushScript {
            local_hash: "abcdef1234567890abcdef1234567890abcdef12",
            push_exit: Exit::Code(128),
            push_stdout: b"",
            push_stderr: b"fatal: could not read from remote repository\n",
            remote_tip: None,
            status_calls: RefCell::default(),
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
    fn a_push_that_exits_zero_but_leaves_the_remote_tip_mismatched_is_refused() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = PushScript {
            local_hash: "abcdef1234567890abcdef1234567890abcdef12",
            push_exit: Exit::Code(0),
            push_stdout: b"",
            push_stderr: b"",
            remote_tip: Some("0000000000000000000000000000000000000"),
            status_calls: RefCell::default(),
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        assert!(reason.contains("exited zero but"), "{reason}");
        assert!(reason.contains("tip is now"), "{reason}");
    }

    #[test]
    fn no_commit_made_leaves_no_push_line_even_with_a_tracked_branch() {
        let journal = journal_of_abc();
        let reporting = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Done,
            exit: Exit::Code(0),
        };
        let commands = CommitScript {
            dirty: false,
            identity: true,
            commit_exit: Exit::Code(0),
            hash: "abc1234",
            calls: RefCell::default(),
            other: &reporting,
        };
        let report = run_queue(
            &journal,
            &clock(),
            &commands,
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
        assert!(
            !commands
                .calls
                .borrow()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("push")),
            "{:?}",
            commands.calls.borrow()
        );
    }
}

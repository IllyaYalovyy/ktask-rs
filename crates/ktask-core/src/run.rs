//! `run`: takes the pending tasks in queue order and attempts each one, once, with the
//! [`Provider`] it is given.

use std::error::Error;
use std::fmt::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use crate::{
    AttemptRun, AttemptToken, BeginAttemptError, Clock, Commands, Journal, JournalError, Outcome,
    Provider, ProviderRunError, RecordReportError, RunLock, RunLockError, Task, TaskId, TaskKind,
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
/// its provider's commands run in, and how long one attempt may run before it is killed.
#[derive(Debug, Clone, Copy)]
pub struct RunContext<'a> {
    /// The project's name.
    pub project_name: &'a str,
    /// The directory the provider's commands run in.
    pub project_dir: &'a Path,
    /// How long one attempt may run before it, and everything it started, is killed.
    pub attempt_timeout: Duration,
}

/// The prompt for attempt `token` of `task`: its title, body and acceptance criteria, and
/// the exact `ktask-rs report` command to run for each possible outcome.
#[must_use]
pub fn build_prompt(task: &Task, token: &AttemptToken) -> String {
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
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         When you are done, run exactly one of these, with the outcome that fits:\n\n\
         \x20\x20\x20\x20ktask-rs report --token {token} done\n\
         \x20\x20\x20\x20ktask-rs report --token {token} failed --reason \"<why>\"\n\
         \x20\x20\x20\x20ktask-rs report --token {token} needs-input --reason \"<why>\"\n\
         \x20\x20\x20\x20ktask-rs report --token {token} too-large --reason \"<why>\"\n"
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
    /// Stop: nothing is pending — `queue_is_empty` says whether the queue holds no tasks at
    /// all, or holds tasks that are all already decided.
    NothingLeft { queue_is_empty: bool },
}

/// Looks at the queue in order and decides what the run does next.
///
/// # Errors
///
/// Fails when the journal cannot be read.
fn pick_next_task(journal: &impl Journal) -> Result<Pick, RunError> {
    let tasks = list_tasks(journal)?;
    let Some(next) = tasks.iter().find(|task| task.status == TaskStatus::Pending) else {
        return Ok(Pick::NothingLeft {
            queue_is_empty: tasks.is_empty(),
        });
    };
    Ok(if next.kind == TaskKind::Human {
        Pick::Human(next.id)
    } else {
        Pick::Task(next.clone())
    })
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

/// Runs `provider` on `prompt` for attempt `token`, timing it from `clock`.
fn timed_provider_run(
    commands: &impl Commands,
    provider: &Provider,
    clock: &impl Clock,
    prompt: &str,
    token: &AttemptToken,
    context: RunContext<'_>,
) -> (Duration, Result<crate::Output, ProviderRunError>) {
    let started = clock.now();
    let result = run_provider(
        commands,
        provider,
        prompt,
        &token.to_string(),
        token.number,
        context.project_dir,
        context.attempt_timeout,
    );
    (
        clock.now().duration_since(started).unwrap_or_default(),
        result,
    )
}

/// Runs one attempt at `task` with `provider`: begins it, builds its prompt, runs the
/// provider, decides the outcome, and ends the attempt with it.
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
) -> Result<Attempted, RunError> {
    let number = crate::attempt::begin_attempt_running(journal, clock, task.id, provider.name)?;
    let token = AttemptToken::new(context.project_name, task.id, number);
    let prompt = build_prompt(task, &token);

    let (duration, result) =
        timed_provider_run(commands, provider, clock, &prompt, &token, context);

    let (exit_code, status, reason) = attempt_outcome(journal, task, &token, result)?;
    crate::attempt::end_attempt(
        journal,
        task.id,
        token.number,
        AttemptRun {
            duration,
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
/// kind `human`, an attempt that does not report `done`, or nothing left pending.
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
                let result = run_one_attempt(journal, clock, commands, provider, context, &task)?;
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
            Pick::NothingLeft { queue_is_empty } => {
                let end = end_when_nothing_left(&attempted, queue_is_empty);
                return Ok(RunReport { attempted, end });
            }
        }
    }
}

/// Use case: runs the pending tasks of `context.project_name`, in queue order, one attempt
/// each, with `provider` — stopping at the first task of kind `human`, at the first attempt
/// that does not report `done`, or when nothing is left pending.
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
    result: Result<crate::Output, ProviderRunError>,
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
        crate::Exit::Code(code) => code,
        crate::Exit::Killed => {
            return Ok((
                None,
                TaskStatus::FailedUnknown,
                Some("the provider ran past its time limit and was killed".to_owned()),
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
        Exit, Outcome, Placement, ProviderCommand, TaskDraft, TaskKind, TaskStatus, add_task,
        report,
    };

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(1_000))
    }

    /// A provider whose command carries the token as `args[1]` (after a placeholder at
    /// `args[0]`, mirroring what a real provider's own flags might occupy) and the attempt as
    /// `args[2]`, and the prompt as its standard input — enough for tests to see what `run`
    /// passed it, without this being any particular real provider.
    fn test_provider() -> Provider {
        Provider {
            name: "test",
            command: |prompt, token, attempt| {
                Ok(ProviderCommand {
                    program: "run-it".to_owned(),
                    args: vec!["-s".to_owned(), token.to_owned(), attempt.to_string()],
                    stdin: prompt.as_bytes().to_vec(),
                })
            },
        }
    }

    fn commands_ok(exit: Exit) -> FakeCommands {
        FakeCommands::returning(Ok(crate::Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit,
        }))
    }

    fn context(timeout: Duration) -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            attempt_timeout: timeout,
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
        let prompt = build_prompt(&task, &token);
        assert!(prompt.contains("Do the thing"), "{prompt}");
        assert!(prompt.contains("Some body text."), "{prompt}");
        assert!(prompt.contains("- first thing"), "{prompt}");
        assert!(prompt.contains("- second thing"), "{prompt}");
        assert!(
            prompt.contains("ktask-rs report --token proj/7/3 done"),
            "{prompt}"
        );
        assert!(
            prompt.contains("ktask-rs report --token proj/7/3 failed --reason"),
            "{prompt}"
        );
        assert!(
            prompt.contains("ktask-rs report --token proj/7/3 needs-input --reason"),
            "{prompt}"
        );
        assert!(
            prompt.contains("ktask-rs report --token proj/7/3 too-large --reason"),
            "{prompt}"
        );
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
        fn run(&self, spec: &crate::CommandSpec) -> Result<crate::Output, crate::CommandsError> {
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
            Ok(crate::Output {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit: self.exit,
            })
        }
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
            command: |_prompt, token, attempt| {
                Ok(ProviderCommand {
                    program: "printf".to_owned(),
                    args: vec![token.to_owned(), format!("attempt={attempt}")],
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
    fn a_provider_that_cannot_be_started_ends_the_task_failed_unknown() {
        let journal = journal_of_abc();
        let commands = FakeCommands::returning(Err(crate::CommandsError::new("bash not found")));
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
            command: |_, _, _| Err("cannot build it".to_owned()),
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
}

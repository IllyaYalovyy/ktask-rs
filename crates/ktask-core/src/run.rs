//! `run`: takes the pending tasks in queue order and attempts each one, once, with the
//! `echo` provider — the only one that exists so far.

use std::error::Error;
use std::fmt::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use crate::{
    AttemptRun, AttemptToken, BeginAttemptError, Clock, Commands, EchoError, Journal, JournalError,
    Outcome, RecordReportError, Task, TaskId, TaskKind, TaskStatus, echo, list_tasks, run_echo,
    start_attempt,
};

/// Why a run could not proceed at all — never for how an attempt itself ended, which is a
/// normal [`RunReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    message: String,
}

impl RunError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RunError {}

impl From<JournalError> for RunError {
    fn from(error: JournalError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<BeginAttemptError> for RunError {
    fn from(error: BeginAttemptError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<RecordReportError> for RunError {
    fn from(error: RecordReportError) -> Self {
        Self::new(error.to_string())
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
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Every task attempted, in the order attempted.
    pub attempted: Vec<Attempted>,
    /// Why the run ended.
    pub end: RunEnd,
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

/// Use case: runs the pending tasks of `project` in queue order, one attempt each, with the
/// `echo` provider, in `project_dir` — stopping at the first task of kind `human`, at the
/// first attempt that does not report `done`, or when nothing is left pending.
///
/// # Errors
///
/// Fails when the journal cannot be read or written; an attempt's own failure is reported
/// in the returned [`RunReport`], not here.
pub fn run_queue(
    journal: &impl Journal,
    clock: &impl Clock,
    commands: &impl Commands,
    project_name: &str,
    project_dir: &Path,
    attempt_timeout: Duration,
) -> Result<RunReport, RunError> {
    let mut attempted = Vec::new();
    loop {
        let tasks = list_tasks(journal)?;
        let Some(next) = tasks.iter().find(|task| task.status == TaskStatus::Pending) else {
            let end = if attempted.is_empty() {
                if tasks.is_empty() {
                    RunEnd::EmptyQueue
                } else {
                    RunEnd::NothingPending
                }
            } else {
                RunEnd::Completed
            };
            return Ok(RunReport { attempted, end });
        };
        if next.kind == TaskKind::Human {
            return Ok(RunReport {
                attempted,
                end: RunEnd::HumanTask(next.id),
            });
        }

        let task = next.clone();
        let token = start_attempt(journal, clock, project_name, task.id)?;
        let prompt = build_prompt(&task, &token);
        journal.attempt_running(task.id, token.number, echo::NAME, clock.now())?;

        let started = clock.now();
        let result = run_echo(
            commands,
            &prompt,
            &token.to_string(),
            token.number,
            project_dir,
            attempt_timeout,
        );
        let ended = clock.now();
        let duration = ended.duration_since(started).unwrap_or_default();

        let (exit_code, status, reason) = attempt_outcome(journal, &task, &token, result)?;

        journal.end_attempt(
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
        attempted.push(Attempted {
            id: task.id,
            status,
            reason,
        });
        if status != TaskStatus::Done {
            return Ok(RunReport {
                attempted,
                end: RunEnd::Stopped {
                    id: task.id,
                    status,
                },
            });
        }
    }
}

/// What attempt `token` of `task` ended at, given what running the provider produced: its
/// exit code (`None` when the provider could not be run at all, or was killed), the
/// resulting status, and the reason when it is not `done`.
fn attempt_outcome(
    journal: &impl Journal,
    task: &Task,
    token: &AttemptToken,
    result: Result<crate::Output, EchoError>,
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
    let report = journal.last_report(task.id, token.number)?;
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

    use crate::fakes::{FakeClock, FakeCommands, FakeJournal, at, draft};
    use crate::{Exit, Outcome, Placement, TaskDraft, TaskKind, TaskStatus, add_task, report};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(1_000))
    }

    /// A valid draft titled `title`, whose body holds a fenced bash block, so the `echo`
    /// provider — which runs the first one it finds in the prompt — has something to run.
    fn agent_draft(title: &str) -> TaskDraft {
        TaskDraft {
            body: "```bash\necho ok\n```\n".to_owned(),
            ..draft(title)
        }
    }

    fn commands_ok(exit: Exit) -> FakeCommands {
        FakeCommands::returning(Ok(crate::Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit,
        }))
    }

    fn run(
        journal: &FakeJournal,
        commands: &impl Commands,
        timeout: Duration,
    ) -> Result<RunReport, RunError> {
        run_queue(
            journal,
            &clock(),
            commands,
            "proj",
            Path::new("/work/proj"),
            timeout,
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
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        journal.tasks.borrow_mut()[0].status = TaskStatus::Done;
        let commands = commands_ok(Exit::Code(0));
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
        assert_eq!(
            report,
            RunReport {
                attempted: vec![],
                end: RunEnd::HumanTask(TaskId(1)),
            }
        );
        assert_eq!(journal.tasks.borrow()[0].status, TaskStatus::Pending);
        assert!(commands.last.borrow().is_none());
    }

    /// A journal with three pending tasks, `a`, `b` and `c`.
    fn journal_of_abc() -> FakeJournal {
        let journal = FakeJournal::default();
        for title in ["a", "b", "c"] {
            add_task(&journal, &clock(), &agent_draft(title), Placement::End).unwrap();
        }
        journal
    }

    /// A fake commands port that, once run, reports `outcome` for whatever token it is given
    /// as `$1`, then exits with `exit`.
    struct ReportingCommands<'a> {
        journal: &'a FakeJournal,
        outcome: Outcome,
        exit: Exit,
    }

    impl Commands for ReportingCommands<'_> {
        fn run(&self, spec: &crate::CommandSpec) -> Result<crate::Output, crate::CommandsError> {
            let token: AttemptToken = spec.args[1].parse().unwrap();
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
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        for task in journal.tasks.borrow().iter() {
            assert_eq!(task.status, TaskStatus::Done, "{}", task.id);
        }
    }

    #[test]
    fn a_failed_report_ends_that_task_failed_and_stops_the_run_leaving_the_rest_pending() {
        let journal = journal_of_abc();
        let commands = ReportingCommands {
            journal: &journal,
            outcome: Outcome::Failed,
            exit: Exit::Code(0),
        };
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        let tasks = journal.tasks.borrow();
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
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        let tasks = journal.tasks.borrow();
        assert_eq!(tasks[0].status, TaskStatus::Blocked);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn no_report_at_all_ends_the_task_failed_unknown_and_stops_the_run() {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Code(0));
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        let tasks = journal.tasks.borrow();
        assert_eq!(tasks[0].status, TaskStatus::FailedUnknown);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn a_killed_provider_ends_the_task_failed_unknown_and_stops_the_run() {
        let journal = journal_of_abc();
        let commands = commands_ok(Exit::Killed);
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
        let report = run(&journal, &commands, Duration::from_secs(60)).unwrap();
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
    fn the_provider_runs_in_the_projects_directory_with_the_attempts_timeout() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &agent_draft("a"), Placement::End).unwrap();
        let commands = commands_ok(Exit::Code(0));
        run_queue(
            &journal,
            &clock(),
            &commands,
            "proj",
            Path::new("/work/proj"),
            Duration::from_secs(42),
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
        let error = run(&journal, &commands, Duration::from_secs(60)).unwrap_err();
        assert_eq!(error.to_string(), failure.to_string());
    }
}

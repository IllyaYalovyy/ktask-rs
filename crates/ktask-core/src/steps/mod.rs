//! The interface every step of a task's attempt is built from — its name, whether it is
//! switched on, running it, and what it recorded — and the list of them one attempt walks.
//!
//! The sync and health-check steps are not on this list. Both run ahead of an attempt even
//! being begun, and their own failure leaves no attempt at all: the task they stop stays
//! `pending`, never `failed`, which no ended attempt could ever leave it at. Each still lives
//! in its own module, [`sync`] and [`health_check`], with its own name, switch and outcome;
//! [`crate::run`] calls them directly, ahead of walking the list built here.

mod agent;
pub(crate) mod commit;
pub(crate) mod health_check;
pub(crate) mod implementation;
pub(crate) mod push;
pub(crate) mod review;
pub(crate) mod sync;
pub(crate) mod test_step;

use std::time::Duration;

pub(crate) use agent::run_agent_step;

use crate::run::Attempted;
use crate::{
    AttemptRun, AttemptToken, Clock, Commands, Git, Journal, Outcome, Provider, RunContext,
    RunError, Task, TaskId, TaskStatus,
};

/// The reason recorded for an attempt a killed run left running, found still running when the
/// next run starts — and for a step whose own provider run was interrupted the same way.
pub(crate) const INTERRUPTED: &str = "the run was interrupted";

/// One step of a task's attempt, run within an attempt already begun and recorded as one of
/// its own steps in the journal: begun, run, ended. No step here names another — each reaches
/// only [`Deps`], [`RunContext`] and [`PipelineState`], which name nothing step-specific.
pub(crate) trait Step {
    /// The step's name, as recorded in the journal.
    fn name(&self) -> &'static str;

    /// Whether the step is switched on: named in `context.disabled_steps`, or — for a step
    /// whose readiness also depends on what an earlier step left in `state` — otherwise ready
    /// to run. A step this returns `false` for is skipped entirely: it leaves no journal event.
    fn enabled(&self, context: RunContext<'_>, state: &PipelineState<'_>) -> bool;

    /// Runs the step, returning what it found and did.
    ///
    /// # Errors
    ///
    /// Fails when the journal cannot be read or written.
    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError>;
}

/// What running one [`Step`] produced.
pub(crate) enum StepOutcome {
    /// It passed. `reason` is `Some` for a step with something worth recording even though it
    /// passed (the commit step's hash, say, or "nothing was changed"); `None` for a step
    /// (implementation, review, test) that says nothing beyond passing.
    Passed {
        /// How long it took.
        duration: Duration,
        /// Its own exit code, when it ran a process.
        exit_code: Option<i32>,
        /// What it has to say even though it passed.
        reason: Option<String>,
        /// The agent's own fine-grained outcome, when this step ran a provider and it reported
        /// one.
        reported: Option<Outcome>,
    },
    /// It ended the whole attempt right here: `status` other than `done`, why, and — for a
    /// step that ran a provider and it reported an outcome of its own — what.
    Ended {
        /// How long it took.
        duration: Duration,
        /// Its own exit code, when it ran a process.
        exit_code: Option<i32>,
        /// What the attempt, and so the task, ends at.
        status: TaskStatus,
        /// Why.
        reason: Option<String>,
        /// The agent's own fine-grained outcome, when this step ran a provider and it reported
        /// one.
        reported: Option<Outcome>,
    },
}

/// The two ports a step's own logic may reach the outside world through, plus the provider and
/// the journal every step's recording needs — bundled so a step takes one argument for them
/// rather than several, and a new one added later costs every existing step nothing.
#[derive(Clone, Copy)]
pub(crate) struct Deps<'a> {
    pub(crate) journal: &'a dyn Journal,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) commands: &'a dyn Commands,
    pub(crate) git: &'a dyn Git,
    pub(crate) provider: &'a Provider,
}

/// What is common to every step of one attempt: the task and the token identifying it, the
/// commit `HEAD` named before the first step ran (the review and test steps' diff is taken
/// against this), the commit the commit step made, once it has run, and the exit code the most
/// recent step that ran a process left — the three a later step may need from an earlier one,
/// carried here rather than one step reading another's own recorded outcome.
pub(crate) struct PipelineState<'a> {
    pub(crate) task: &'a Task,
    pub(crate) token: &'a AttemptToken,
    /// `None` when it could not be captured: the review and test steps' diff is then empty
    /// rather than the run failing over it.
    pub(crate) start_commit: Option<String>,
    /// The commit the commit step made, once it has run and there was something to commit.
    pub(crate) committed: Option<String>,
    /// The most recent process-running step's own exit code — never touched by a step, such as
    /// the commit or push step, that runs no process of its own.
    pub(crate) exit_code: Option<i32>,
}

/// One step that already ran and passed before the attempt it belongs to was even begun — the
/// sync and health-check steps, when the project has configured and switched them on —
/// recorded as the attempt's own first steps, in the order they ran, once it begins.
pub(crate) struct PreStep {
    /// The step's name: [`crate::SYNC_STEP`] or [`crate::HEALTH_CHECK_STEP`].
    pub(crate) name: &'static str,
    /// How long it took.
    pub(crate) duration: Duration,
    /// What it has to say even though it passed — the sync step's own message, or `None` for
    /// the health check, which has nothing to add.
    pub(crate) reason: Option<String>,
}

/// The steps every attempt walks, once its sync and health-check gates — run ahead of it,
/// outside this list — have passed: implementation, then, when switched on, review, then, when
/// switched on, test; then, when switched on, commit, then, when the project tracks a branch,
/// a commit was made and push is switched on, push. Implementation is never switched off.
pub(crate) fn default_steps() -> Vec<Box<dyn Step>> {
    vec![
        Box::new(implementation::Implementation),
        Box::new(review::Review),
        Box::new(test_step::Test),
        Box::new(commit::Commit),
        Box::new(push::Push),
    ]
}

/// The commit `HEAD` names in `context`'s project directory, right now. `None` when git could
/// not answer.
pub(crate) fn current_commit(git: &dyn Git, context: RunContext<'_>) -> Option<String> {
    git.head(context.project_dir)
}

/// Everything changed in `context`'s project directory since `start_commit`, committed or still
/// sitting uncommitted in the working tree. Empty when `start_commit` is `None`, or git could
/// not produce one.
pub(crate) fn diff_since(
    git: &dyn Git,
    context: RunContext<'_>,
    start_commit: Option<&str>,
) -> String {
    let Some(start_commit) = start_commit else {
        return String::new();
    };
    git.diff_since(context.project_dir, start_commit)
}

/// Records a step, named `step`, that already ran and passed, in `duration`, with `reason` —
/// `Some` when it has something to say even though it passed — as one of the steps of attempt
/// `number` of task `id`'s own steps ahead of the list's own: begun and ended in the same call,
/// since it ran before the attempt itself was begun.
fn record_passed_step(
    journal: &dyn Journal,
    clock: &dyn Clock,
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

/// `outcome`'s duration, exit code, status (`done` for [`StepOutcome::Passed`]), reason and
/// reported outcome, whichever of the two it is.
fn outcome_fields(
    outcome: StepOutcome,
) -> (
    Duration,
    Option<i32>,
    TaskStatus,
    Option<String>,
    Option<Outcome>,
) {
    match outcome {
        StepOutcome::Passed {
            duration,
            exit_code,
            reason,
            reported,
        } => (duration, exit_code, TaskStatus::Done, reason, reported),
        StepOutcome::Ended {
            duration,
            exit_code,
            status,
            reason,
            reported,
        } => (duration, exit_code, status, reason, reported),
    }
}

/// Begins, runs and ends one `step`, already known enabled: its duration, the status it ended
/// at, and, when that is not `done`, why.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn run_one_step(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &dyn Step,
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    crate::attempt::begin_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step.name(),
    )?;
    let (duration, exit_code, status, reason, reported) =
        outcome_fields(step.run(deps, context, state)?);
    crate::attempt::end_step(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        step.name(),
        AttemptRun {
            duration,
            exit_code,
            status,
            reason: reason.as_deref(),
        },
        reported,
    )?;
    Ok((duration, status, reason))
}

/// Walks `steps`, in order: skips a disabled one entirely, otherwise runs it via
/// [`run_one_step`] — stopping at the first that does not pass, so a step after it is never
/// begun and leaves no event in the journal. Returns the steps' combined duration and the
/// outcome of the last one run, which becomes the whole attempt's; a step that only passes
/// never changes either, whatever it recorded for its own step line.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn run_attempt_steps(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    steps: &[Box<dyn Step>],
) -> Result<(Duration, TaskStatus, Option<String>), RunError> {
    let mut total = Duration::ZERO;
    let mut status = TaskStatus::Done;
    let mut reason = None;
    for step in steps {
        if !step.enabled(context, state) {
            continue;
        }
        let (duration, step_status, step_reason) =
            run_one_step(deps, context, state, step.as_ref())?;
        total += duration;
        if step_status != TaskStatus::Done {
            status = step_status;
            reason = step_reason;
            break;
        }
    }
    Ok((total, status, reason))
}

/// Records every one of `pre_steps` as attempt `number` of task `id`'s own first steps, in
/// order, via [`record_passed_step`]; their combined duration.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn record_pre_steps(
    journal: &dyn Journal,
    clock: &dyn Clock,
    id: TaskId,
    number: u32,
    pre_steps: &[PreStep],
) -> Result<Duration, RunError> {
    let mut total = Duration::ZERO;
    for pre_step in pre_steps {
        record_passed_step(
            journal,
            clock,
            id,
            number,
            pre_step.name,
            pre_step.duration,
            pre_step.reason.as_deref(),
        )?;
        total += pre_step.duration;
    }
    Ok(total)
}

/// Runs one attempt at `task` with `deps.provider`: begins it, records `pre_steps` as the
/// attempt's own first steps in order, then walks `steps`, and ends the attempt with the
/// outcome they left it at.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
/// Builds the pipeline state for attempt `number`, walks `steps` through it, and ends the
/// attempt with the outcome they left it at, `pre_duration` already spent on its pre-steps.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn finish_attempt(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    number: u32,
    pre_duration: Duration,
    steps: &[Box<dyn Step>],
) -> Result<Attempted, RunError> {
    let start_commit = current_commit(deps.git, context);
    let token = AttemptToken::new(context.project_name, task.id, number);
    let mut state = PipelineState {
        task,
        token: &token,
        start_commit,
        committed: None,
        exit_code: None,
    };
    let (steps_duration, status, reason) = run_attempt_steps(&deps, context, &mut state, steps)?;
    crate::attempt::end_attempt(
        deps.journal,
        task.id,
        number,
        AttemptRun {
            duration: pre_duration + steps_duration,
            exit_code: state.exit_code,
            status,
            reason: reason.as_deref(),
        },
        deps.clock.now(),
    )?;
    Ok(Attempted {
        id: task.id,
        status,
        reason,
    })
}

pub(crate) fn run_one_attempt(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    pre_steps: &[PreStep],
    steps: &[Box<dyn Step>],
) -> Result<Attempted, RunError> {
    let number = crate::attempt::begin_attempt_running(
        deps.journal,
        deps.clock,
        task.id,
        deps.provider.name,
    )?;
    let pre_duration = record_pre_steps(deps.journal, deps.clock, task.id, number, pre_steps)?;
    finish_attempt(deps, context, task, number, pre_duration, steps)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fakes::{FakeClock, FakeCommands, FakeGit, FakeJournal, at, draft};
    use crate::{Exit, Output, Placement, ProviderCommand, add_task, list_all_tasks};

    fn clock() -> FakeClock {
        FakeClock(at(1_000))
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

    fn test_provider() -> Provider {
        Provider {
            name: "test",
            command: |_prompt, _call| {
                Ok(ProviderCommand {
                    program: "true".to_owned(),
                    args: vec![],
                    stdin: vec![],
                })
            },
        }
    }

    /// A step defined only by this test: it always ends the attempt at `status`, with no
    /// journal event beyond its own. Stands in for a step no module here ships, to prove the
    /// list [`default_steps`] builds is not the only one [`run_one_attempt`] can walk.
    struct Fixed {
        name: &'static str,
        status: TaskStatus,
        /// What it records for its own step line even though it passes — never the whole
        /// attempt's own reason, which stays `None` unless a step ends badly.
        pass_reason: Option<&'static str>,
    }

    impl Step for Fixed {
        fn name(&self) -> &'static str {
            self.name
        }

        fn enabled(&self, _context: RunContext<'_>, _state: &PipelineState<'_>) -> bool {
            true
        }

        fn run(
            &self,
            _deps: &Deps<'_>,
            _context: RunContext<'_>,
            _state: &mut PipelineState<'_>,
        ) -> Result<StepOutcome, RunError> {
            Ok(if self.status == TaskStatus::Done {
                StepOutcome::Passed {
                    duration: Duration::ZERO,
                    exit_code: Some(0),
                    reason: self.pass_reason.map(str::to_owned),
                    reported: None,
                }
            } else {
                StepOutcome::Ended {
                    duration: Duration::ZERO,
                    exit_code: None,
                    status: self.status,
                    reason: Some("stopped".to_owned()),
                    reported: None,
                }
            })
        }
    }

    /// One task, `a`, added to a fresh journal.
    fn task_a(journal: &FakeJournal) -> Task {
        add_task(journal, &clock(), &draft("a"), Placement::End).unwrap();
        list_all_tasks(journal).unwrap().remove(0)
    }

    #[test]
    fn a_step_a_test_defines_joins_the_list_and_runs_like_any_other() {
        let journal = FakeJournal::default();
        let task = task_a(&journal);
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        let steps: Vec<Box<dyn Step>> = vec![Box::new(Fixed {
            name: "extra",
            status: TaskStatus::Done,
            pass_reason: None,
        })];
        let (clock, git, provider) = (clock(), FakeGit::default(), test_provider());
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
        };
        let attempted =
            run_one_attempt(deps, context(Duration::from_secs(60)), &task, &[], &steps).unwrap();
        assert_eq!(attempted.status, TaskStatus::Done);
        assert_eq!(attempted.reason, None);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, "extra");
        assert_eq!(
            attempt.steps[0].ended.as_ref().unwrap().status,
            TaskStatus::Done
        );
    }

    #[test]
    fn a_step_that_ends_badly_stops_the_list_and_later_steps_leave_no_event() {
        let journal = FakeJournal::default();
        let task = task_a(&journal);
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        let steps: Vec<Box<dyn Step>> = vec![
            Box::new(Fixed {
                name: "one",
                status: TaskStatus::Failed,
                pass_reason: None,
            }),
            Box::new(Fixed {
                name: "two",
                status: TaskStatus::Done,
                pass_reason: None,
            }),
        ];
        let (clock, git, provider) = (clock(), FakeGit::default(), test_provider());
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
        };
        let attempted =
            run_one_attempt(deps, context(Duration::from_secs(60)), &task, &[], &steps).unwrap();
        assert_eq!(attempted.status, TaskStatus::Failed);
        assert_eq!(attempted.reason, Some("stopped".to_owned()));
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        // Only the first, failing step ran: the second is never begun, so it leaves no event.
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, "one");
    }

    #[test]
    fn a_passing_step_never_leaks_its_own_reason_into_the_attempts() {
        let journal = FakeJournal::default();
        let task = task_a(&journal);
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        let steps: Vec<Box<dyn Step>> = vec![Box::new(Fixed {
            name: "one",
            status: TaskStatus::Done,
            pass_reason: Some("something worth noting"),
        })];
        let (clock, git, provider) = (clock(), FakeGit::default(), test_provider());
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
        };
        let attempted =
            run_one_attempt(deps, context(Duration::from_secs(60)), &task, &[], &steps).unwrap();
        assert_eq!(attempted.reason, None);
    }
}

//! The interface every step of a task's attempt is built from — its name, whether it is
//! switched on, running it, and what it recorded — and the list of them one attempt walks.
//!
//! The sync and health-check steps are not on this list. Both run ahead of an attempt even
//! being begun, and their own failure leaves no attempt at all: the task they stop stays
//! `pending`, never `failed`, which no ended attempt could ever leave it at. Each still lives
//! in its own module, [`sync`] and [`health_check`], with its own name, switch and outcome;
//! [`crate::run`] calls them directly, ahead of walking the list built here.

mod agent;
mod attempt_count;
pub(crate) mod check;
pub(crate) mod commit;
mod execute;
pub(crate) mod health_check;
pub(crate) mod implementation;
pub(crate) mod instructions;
mod outcome;
mod pipeline_state;
pub(crate) mod push;
pub(crate) mod resolve;
pub(crate) mod review;
pub(crate) mod sync;
pub(crate) mod test_step;

use std::time::Duration;

pub(crate) use agent::run_agent_step;
pub(crate) use execute::{record_pre_steps, run_one_step};
pub(crate) use outcome::StepOutcome;
pub(crate) use pipeline_state::PipelineState;

use crate::run::Attempted;
use crate::{
    AttemptOutput, AttemptToken, Clock, Commands, Git, Journal, Provider, RunContext, RunError,
    SessionLog, Sleep, Task, TaskStatus,
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

    /// The model this step runs with, recorded alongside it when it begins. `None` for every
    /// step but the resolve step, which names the project's own `resolver-model` setting, when
    /// it has set one, and the implementation step, which names `state.requested_model`, when
    /// the resolver's own `retry` decision named one for this attempt.
    fn model(&self, _context: RunContext<'_>, _state: &PipelineState<'_>) -> Option<String> {
        None
    }

    /// The reason the attempt ends with when this step fails with `step_reason`; the step's own
    /// reason by default.
    fn attempt_reason(&self, step_reason: Option<String>) -> Option<String> {
        step_reason
    }

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
    /// The provider the resolve step uses; every other agent step uses `provider`.
    pub(crate) resolver_provider: &'a Provider,
    pub(crate) session_log: &'a dyn SessionLog,
    pub(crate) sleep: &'a dyn Sleep,
    pub(crate) output: &'a dyn AttemptOutput,
    /// What each agent step's prompt opens with, read by the instructions gate for this task.
    pub(crate) instructions: &'a instructions::Instructions,
}

impl Deps<'_> {
    /// The provider that actually runs `step`.
    pub(crate) fn provider_for(&self, step: &str) -> &Provider {
        if step == crate::RESOLVE_STEP {
            self.resolver_provider
        } else {
            self.provider
        }
    }
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
/// outside this list — have passed: implementation, then, when a check command is set and the step is switched on, check, then, when switched on, review, then, when
/// switched on, test; then, when switched on, commit, then, when the project tracks a branch,
/// a commit was made and push is switched on, push. Implementation is never switched off.
pub(crate) fn default_steps() -> Vec<Box<dyn Step>> {
    vec![
        Box::new(implementation::Implementation),
        Box::new(check::Check),
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
            reason = step.attempt_reason(step_reason);
            break;
        }
    }
    Ok((total, status, reason))
}

/// Ends attempt `number` of `task` at `status` (with `reason`, `exit_code` and `duration`)
/// directly, the resolver never eligible to run — [`finish_attempt`]'s own tail, pulled out of
/// it so it stays within the workspace's function-length limit.
fn end_attempt_plainly(
    deps: Deps<'_>,
    task: &Task,
    number: u32,
    duration: Duration,
    exit_code: Option<i32>,
    status: TaskStatus,
    reason: Option<String>,
) -> Result<Attempted, RunError> {
    resolve::end_attempt_now(
        deps,
        task.id,
        number,
        duration,
        exit_code,
        status,
        reason.as_deref(),
    )?;
    Ok(Attempted {
        id: task.id,
        status,
        reason,
    })
}

/// Builds the pipeline state for attempt `number`, walks `steps` through it, and ends it, or
/// hands it to the resolver — `pre_duration` already spent on its pre-steps.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
#[allow(clippy::too_many_arguments)]
fn finish_attempt(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    number: u32,
    start_commit: Option<String>,
    pre_duration: Duration,
    steps: &[Box<dyn Step>],
    requested_model: Option<String>,
    requested_session: Option<String>,
    extra_time: Duration,
) -> Result<Attempted, RunError> {
    let token = AttemptToken::new(context.project_name, task.id, number);
    let mut state = PipelineState {
        task,
        token: &token,
        start_commit,
        committed: None,
        exit_code: None,
        failure: None,
        requested_model,
        requested_session,
        known_cause: false,
        decision: None,
        extra_time,
        usage: crate::Usage::default(),
        used_model: None,
        limit_warning: None,
    };
    let (steps_duration, status, reason) = run_attempt_steps(&deps, context, &mut state, steps)?;
    let duration = pre_duration + steps_duration;
    end_or_resolve(
        deps, context, task, number, duration, &mut state, status, reason, steps,
    )
}

/// Ends attempt `number` with what its steps left it at — `pending`, without ever reaching the
/// resolver and never weighed against `max_attempts`, when [`PipelineState::known_cause`] says
/// the tool recognised the failure on its own; otherwise the resolver's own job when it is
/// still eligible, [`attempt_count::real_attempt_count`] rather than `number` itself judging
/// that, so a known cause's own attempt is never one of the task's. [`finish_attempt`]'s own
/// tail, pulled out of it so it stays within the workspace's function-length limit.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
#[allow(clippy::too_many_arguments)]
fn end_or_resolve(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    number: u32,
    duration: Duration,
    state: &mut PipelineState<'_>,
    status: TaskStatus,
    reason: Option<String>,
    steps: &[Box<dyn Step>],
) -> Result<Attempted, RunError> {
    if state.known_cause {
        return end_attempt_plainly(
            deps,
            task,
            number,
            duration,
            state.exit_code,
            TaskStatus::Pending,
            reason,
        );
    }
    let real_number = attempt_count::real_attempt_count(deps.journal, task.id, number)?;
    if resolve::resolver_eligible(status, real_number, context.max_attempts) {
        return resolve::resolve_and_continue(
            deps, context, task, number, duration, state, status, reason, steps,
        );
    }
    end_attempt_plainly(
        deps,
        task,
        number,
        duration,
        state.exit_code,
        status,
        reason,
    )
}

/// Begins and runs one attempt at `task`, with `requested_model` — the model the resolver's
/// own `retry` decision named for it, when this is the attempt that decision began; `None` for
/// a task's first attempt, and for a retry that named none.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_one_attempt(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    pre_steps: &[PreStep],
    steps: &[Box<dyn Step>],
    requested_model: Option<String>,
    requested_session: Option<String>,
    extra_time: Duration,
) -> Result<Attempted, RunError> {
    let start_commit = current_commit(deps.git, context);
    let number = crate::attempt::begin_attempt_running(
        deps.journal,
        deps.clock,
        task.id,
        &deps.provider.name,
        start_commit.as_deref(),
    )?;
    let pre_duration = record_pre_steps(deps.journal, deps.clock, task.id, number, pre_steps)?;
    finish_attempt(
        deps,
        context,
        task,
        number,
        start_commit,
        pre_duration,
        steps,
        requested_model,
        requested_session,
        extra_time,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fakes::{
        FakeClock, FakeCommands, FakeGit, FakeJournal, FakeSessionLog, FakeSleep, at, draft,
    };
    use crate::route::Signals;
    use crate::{Exit, Output, Placement, ProviderCommand, TaskId, add_task, list_all_tasks};

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
            check_command: None,
            tracked_branch: None,
            disabled_steps: &[],
            max_attempts: 1,
            transport_retries: 3,
            model: "",
            resolver_model: "",
            sessions_dir: Path::new("/state/sessions"),
            outputs_dir: Path::new("/state/outputs"),
            instructions_dir: "docs",
        }
    }

    fn test_provider() -> Provider {
        Provider {
            name: "test".to_owned(),
            command: std::sync::Arc::new(|_prompt, _call| {
                Ok(ProviderCommand {
                    program: "true".to_owned(),
                    args: vec![],
                    stdin: vec![],
                })
            }),
            supports_resume: false,
            read_session: std::sync::Arc::new(|_| None),
            detect_limit: std::sync::Arc::new(|_| None),
            parse_output: std::sync::Arc::new(|output| output),
            read_usage: std::sync::Arc::new(|_| crate::ProviderUsage::default()),
            model_aliases: std::collections::BTreeMap::new(),
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
                    signals: Signals::default(),
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
        let (clock, git, provider, session_log, sleep) = (
            clock(),
            FakeGit::default(),
            test_provider(),
            FakeSessionLog::default(),
            FakeSleep::default(),
        );
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
            resolver_provider: &provider,
            session_log: &session_log,
            sleep: &sleep,
            output: &crate::NoAttemptOutput,
            instructions: &instructions::Instructions::default(),
        };
        let attempted = run_one_attempt(
            deps,
            context(Duration::from_secs(60)),
            &task,
            &[],
            &steps,
            None,
            None,
            Duration::ZERO,
        )
        .unwrap();
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
        let (clock, git, provider, session_log, sleep) = (
            clock(),
            FakeGit::default(),
            test_provider(),
            FakeSessionLog::default(),
            FakeSleep::default(),
        );
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
            resolver_provider: &provider,
            session_log: &session_log,
            sleep: &sleep,
            output: &crate::NoAttemptOutput,
            instructions: &instructions::Instructions::default(),
        };
        let attempted = run_one_attempt(
            deps,
            context(Duration::from_secs(60)),
            &task,
            &[],
            &steps,
            None,
            None,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(attempted.status, TaskStatus::Failed);
        assert_eq!(attempted.reason, Some("stopped".to_owned()));
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        // Only the first, failing step ran: the second is never begun, so it leaves no event.
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        assert_eq!(attempt.steps[0].name, "one");
    }

    /// A step whose provider reports its usage limit was hit once, then passes — stands in for
    /// what [`agent::run_agent_step`] itself does on a real one, without running a real
    /// provider.
    struct WaitsOnce {
        waited: std::cell::Cell<bool>,
    }

    impl Step for WaitsOnce {
        fn name(&self) -> &'static str {
            "extra"
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
            Ok(if self.waited.replace(true) {
                StepOutcome::Passed {
                    duration: Duration::from_secs(5),
                    exit_code: Some(0),
                    reason: None,
                    reported: None,
                }
            } else {
                StepOutcome::Ended {
                    duration: Duration::from_secs(2),
                    exit_code: Some(1),
                    status: TaskStatus::Failed,
                    reason: None,
                    reported: None,
                    signals: Signals {
                        limit: Some(crate::LimitSignal {
                            reset_at: Some(at(1_030)),
                        }),
                        ..Signals::default()
                    },
                }
            })
        }
    }

    #[test]
    fn a_step_that_waits_for_its_providers_limit_folds_the_wait_into_its_duration_and_records_it() {
        let journal = FakeJournal::default();
        let task = task_a(&journal);
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        let steps: Vec<Box<dyn Step>> = vec![Box::new(WaitsOnce {
            waited: std::cell::Cell::new(false),
        })];
        let (clock, git, provider, session_log, sleep) = (
            clock(),
            FakeGit::default(),
            test_provider(),
            FakeSessionLog::default(),
            FakeSleep::default(),
        );
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
            resolver_provider: &provider,
            session_log: &session_log,
            sleep: &sleep,
            output: &crate::NoAttemptOutput,
            instructions: &instructions::Instructions::default(),
        };
        let attempted = run_one_attempt(
            deps,
            context(Duration::from_secs(60)),
            &task,
            &[],
            &steps,
            None,
            None,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(attempted.status, TaskStatus::Done);
        let attempt = crate::attempt::last_attempt(&journal, TaskId(1))
            .unwrap()
            .unwrap();
        // One attempt only: the wait never began a fresh one.
        assert_eq!(attempt.number, 1);
        assert_eq!(attempt.steps.len(), 1, "{:?}", attempt.steps);
        let end = attempt.steps[0].ended.as_ref().unwrap();
        // The 2s the provider ran before reporting the limit, the 30s waited out, and the 5s
        // the step then actually took — all three counted in the step's own duration, so the
        // wait is counted in the task's own time even though it spent no attempt on it.
        assert_eq!(end.duration, Duration::from_secs(37));
        assert_eq!(
            end.limit_wait,
            Some(crate::LimitWait {
                waited: Duration::from_secs(30),
                resumed_at: at(1_030),
            })
        );
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
        let (clock, git, provider, session_log, sleep) = (
            clock(),
            FakeGit::default(),
            test_provider(),
            FakeSessionLog::default(),
            FakeSleep::default(),
        );
        let deps = Deps {
            journal: &journal,
            clock: &clock,
            commands: &commands,
            git: &git,
            provider: &provider,
            resolver_provider: &provider,
            session_log: &session_log,
            sleep: &sleep,
            output: &crate::NoAttemptOutput,
            instructions: &instructions::Instructions::default(),
        };
        let attempted = run_one_attempt(
            deps,
            context(Duration::from_secs(60)),
            &task,
            &[],
            &steps,
            None,
            None,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(attempted.reason, None);
    }
}

//! The resolve step: once an attempt has ended `failed` or `failed-unknown`, and the task has
//! not yet used up its `max-attempts`, runs the provider in the resolve role to decide whether
//! a fresh attempt is worth trying.

use std::time::Duration;

use crate::run::Attempted;
use crate::steps::implementation::earlier_attempts;
use crate::steps::{Deps, PipelineState, Step, StepOutcome, run_agent_step, run_one_step};
use crate::{AttemptRun, Journal, RunContext, RunError, Task, TaskId, TaskStatus};

/// The journal name of the resolve step.
pub const RESOLVE_STEP: &str = "resolve";

mod prompt;

use prompt::build_resolve_prompt;

/// The resolve step of a task's attempt: run only when the step ahead of it in the pipeline
/// ended `failed` or `failed-unknown` and the task has attempts left — [`crate::steps::mod`]'s
/// own job to decide, since this step runs outside the pipeline's own list.
pub(crate) struct Resolve;

impl Step for Resolve {
    fn name(&self) -> &'static str {
        RESOLVE_STEP
    }

    fn enabled(&self, _context: RunContext<'_>, _state: &PipelineState<'_>) -> bool {
        true
    }

    fn model(&self, context: RunContext<'_>, _state: &PipelineState<'_>) -> Option<String> {
        (!context.resolver_model.is_empty()).then(|| context.resolver_model.to_owned())
    }

    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError> {
        let (current_status, current_reason) = state
            .failure
            .clone()
            .unwrap_or((TaskStatus::FailedUnknown, None));
        let earlier = earlier_attempts(deps.journal, state.task.id, state.token.number)?;
        let diff = crate::steps::implementation::diff_since_first_attempt(
            deps.journal,
            deps.git,
            context,
            state.task.id,
        )?;
        let prompt = build_resolve_prompt(
            state.task,
            state.token,
            context.binary_path,
            &earlier,
            current_status,
            current_reason.as_deref(),
            &diff,
        );
        let model = self.model(context, state);
        let outcome = run_agent_step(
            deps,
            context,
            state,
            RESOLVE_STEP,
            model.as_deref(),
            &prompt,
        )?;
        mark_reset_tree(deps.journal, state, outcome)
    }
}

/// `outcome`, with its own reason set to say the working tree was reset, when the resolver's
/// `retry` decision for `state`'s attempt asked for `--reset-tree`. Every other outcome —
/// including a `retry` that did not ask for it — is returned untouched.
fn mark_reset_tree(
    journal: &dyn Journal,
    state: &PipelineState<'_>,
    outcome: StepOutcome,
) -> Result<StepOutcome, RunError> {
    let StepOutcome::Passed {
        duration,
        exit_code,
        reported,
        ..
    } = outcome
    else {
        return Ok(outcome);
    };
    let reset = crate::attempt::last_retry_reset_tree(journal, state.task.id, state.token.number)?;
    let reason = reset
        .then(|| "the working tree was reset to the commit this attempt started from".to_owned());
    Ok(StepOutcome::Passed {
        duration,
        exit_code,
        reason,
        reported,
    })
}

/// Ends attempt `number` of `task_id` at `status` (with `reason`), having run for `duration`
/// with `exit_code`, whichever of its own pipeline or the resolve step's decided it.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn end_attempt_now(
    deps: Deps<'_>,
    task_id: TaskId,
    number: u32,
    duration: Duration,
    exit_code: Option<i32>,
    status: TaskStatus,
    reason: Option<&str>,
) -> Result<(), RunError> {
    crate::attempt::end_attempt(
        deps.journal,
        task_id,
        number,
        AttemptRun {
            duration,
            exit_code,
            status,
            reason,
        },
        deps.clock.now(),
    )?;
    Ok(())
}

/// The session the resolver's `retry` decision for attempt `number` of task `id` asked its
/// next attempt to resume: the one that attempt itself ran in, when `retry --same-session`
/// asked for it; `None` when it did not, or that attempt reported no session at all.
///
/// # Errors
///
/// Fails when the journal cannot be read.
fn requested_session(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<Option<String>, RunError> {
    if crate::attempt::last_retry_same_session(journal, id, number)? {
        Ok(crate::attempt::last_session(journal, id, number)?)
    } else {
        Ok(None)
    }
}

/// The commit the working tree should be reset to before task `id`'s next attempt begins:
/// `start_commit`, attempt `number`'s own, when its resolver's `retry` decision asked for
/// `--reset-tree`; `None` when it did not, or `start_commit` itself could not be captured.
///
/// # Errors
///
/// Fails when the journal cannot be read.
fn requested_reset_to(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
    start_commit: Option<&str>,
) -> Result<Option<String>, RunError> {
    if crate::attempt::last_retry_reset_tree(journal, id, number)? {
        Ok(start_commit.map(str::to_owned))
    } else {
        Ok(None)
    }
}

/// Sends task `id` back to `pending`, the same transition [`crate::retry_task`] makes by hand,
/// now made by the resolver's own `retry` decision, so the task's next attempt begins exactly
/// as a hand-retried one would.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn retry_for_resolver(deps: Deps<'_>, id: TaskId) -> Result<(), RunError> {
    crate::queue_state::decide_and_append(deps.journal, |state| {
        state
            .decide_retry(id, deps.clock.now())
            .map(|event| (vec![event], ()))
    })
    .map_err(|error: crate::RetryError| RunError::Other(error.to_string()))
}

/// Whether the resolve step runs after an attempt's pipeline ended at `status`: only after
/// `failed` or `failed-unknown` — never `blocked`, which waits on the operator's own answer
/// instead — and only while `number`, the attempt that just ended, is under `max_attempts`.
pub(crate) fn resolver_eligible(status: TaskStatus, number: u32, max_attempts: u32) -> bool {
    matches!(status, TaskStatus::Failed | TaskStatus::FailedUnknown) && number < max_attempts
}

/// Runs the resolve step for the attempt that just ended at `status` (with `reason`), then
/// ends it and, depending on what the resolver decided, either begins a fresh attempt at the
/// implementation step (`retry`, the resolve step's own outcome `done`) or leaves the task at
/// the resolver's own decision (`stop`, or a resolver that crashed, timed out or reported
/// nothing — both `failed` or `failed-unknown`, the resolve step's own outcome).
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
/// Runs the resolve step itself and ends the attempt at whatever it, or the step ahead of it,
/// decided — [`resolve_and_continue`]'s own first half, pulled out of it so it stays within the
/// workspace's function-length limit. Returns whether the resolver decided `retry`, and the
/// status and reason the attempt ended at.
#[allow(clippy::too_many_arguments)]
fn run_resolve_and_end_attempt(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    number: u32,
    duration_so_far: Duration,
    state: &mut PipelineState<'_>,
    status: TaskStatus,
    reason: Option<String>,
) -> Result<(bool, TaskStatus, Option<String>), RunError> {
    state.failure = Some((status, reason.clone()));
    let (resolve_duration, resolve_status, resolve_reason) =
        run_one_step(&deps, context, state, &Resolve)?;
    let retried = resolve_status == TaskStatus::Done;
    let (end_status, end_reason) = if retried {
        (status, reason)
    } else {
        (resolve_status, resolve_reason)
    };
    let total = duration_so_far + resolve_duration;
    end_attempt_now(
        deps,
        task.id,
        number,
        total,
        state.exit_code,
        end_status,
        end_reason.as_deref(),
    )?;
    Ok((retried, end_status, end_reason))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_and_continue(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    number: u32,
    duration_so_far: Duration,
    state: &mut PipelineState<'_>,
    status: TaskStatus,
    reason: Option<String>,
    steps: &[Box<dyn Step>],
) -> Result<Attempted, RunError> {
    let (retried, end_status, end_reason) = run_resolve_and_end_attempt(
        deps,
        context,
        task,
        number,
        duration_so_far,
        state,
        status,
        reason,
    )?;
    if retried {
        return retry_the_task_after_resolver(deps, context, task, number, state, steps);
    }
    Ok(Attempted {
        id: task.id,
        status: end_status,
        reason: end_reason,
    })
}

/// Reads back what the resolver's own `retry` decision for attempt `number` of `task` asked
/// for — a different model, resuming its own session, resetting the working tree to `state`'s
/// own start commit — and begins the task's next attempt accordingly.
///
/// # Errors
///
/// Fails when the journal cannot be read or written, or the working tree could not be reset.
fn retry_the_task_after_resolver(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    number: u32,
    state: &PipelineState<'_>,
    steps: &[Box<dyn Step>],
) -> Result<Attempted, RunError> {
    let requested_model = crate::attempt::last_retry_model(deps.journal, task.id, number)?;
    let requested_session = requested_session(deps.journal, task.id, number)?;
    let reset_to =
        requested_reset_to(deps.journal, task.id, number, state.start_commit.as_deref())?;
    retry_the_task(
        deps,
        context,
        task,
        steps,
        requested_model,
        requested_session,
        reset_to.as_deref(),
    )
}

/// Sends `task` back to `pending`, the resolver's own `retry` decision, and begins its next
/// attempt at the implementation step at once — with `requested_model`, when the resolver named
/// one, `requested_session`, when the resolver's `retry --same-session` asked the task's next
/// attempt to resume the one that just ended, and `reset_to`, the commit the working tree is
/// returned to first, when the resolver's `retry --reset-tree` asked for it and this attempt's
/// own start commit could be captured — never the sync or health-check gates, which run only
/// once, ahead of a task's very first attempt.
///
/// # Errors
///
/// Fails when the journal cannot be read or written, or the working tree could not be reset.
#[allow(clippy::too_many_arguments)]
fn retry_the_task(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    steps: &[Box<dyn Step>],
    requested_model: Option<String>,
    requested_session: Option<String>,
    reset_to: Option<&str>,
) -> Result<Attempted, RunError> {
    if let Some(commit) = reset_to {
        deps.git
            .reset_tree(context.project_dir, commit)
            .map_err(|error| RunError::Other(error.to_string()))?;
    }
    retry_for_resolver(deps, task.id)?;
    super::run_one_attempt(
        deps,
        context,
        task,
        &[],
        steps,
        requested_model,
        requested_session,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fakes::at;
    use crate::steps::implementation::EarlierAttempt;
    use crate::{AttemptToken, TaskId, TaskKind};

    fn task() -> Task {
        Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: "Some body text.".to_owned(),
            criteria: vec!["first thing".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        }
    }

    #[test]
    fn the_prompt_carries_every_attempt_this_one_included_the_diff_and_the_exact_report_commands() {
        let token = AttemptToken::new("proj", TaskId(7), 3);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let earlier = vec![EarlierAttempt {
            number: 1,
            outcome: "failed".to_owned(),
            reason: Some("it broke".to_owned()),
        }];
        let diff = "--- a/file\n+++ b/file\n+added line\n";
        let prompt = build_resolve_prompt(
            &task(),
            &token,
            binary_path,
            &earlier,
            TaskStatus::FailedUnknown,
            Some("crashed"),
            diff,
        );
        assert!(prompt.contains("Do the thing"), "{prompt}");
        assert!(
            prompt.contains("- attempt 1: failed — it broke"),
            "{prompt}"
        );
        assert!(
            prompt.contains("- attempt 3: failed-unknown — crashed"),
            "{prompt}"
        );
        assert!(prompt.contains("+added line"), "{prompt}");
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 retry"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 stop --reason"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 skip --reason"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 supersede --tasks"),
            "{prompt}"
        );
        assert!(!prompt.contains("\n    ktask-rs report"), "{prompt}");
    }

    #[test]
    fn the_resolve_step_is_always_enabled_and_names_the_resolver_model() {
        let context = RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: Duration::from_secs(60),
            health_check_command: None,
            tracked_branch: None,
            disabled_steps: &[],
            max_attempts: 3,
            transport_retries: 3,
            model: "",
            resolver_model: "opus",
            sessions_dir: Path::new("/state/sessions"),
            outputs_dir: Path::new("/state/outputs"),
        };
        let state = PipelineState {
            task: &task(),
            token: &AttemptToken::new("proj", TaskId(7), 1),
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
        assert!(Resolve.enabled(context, &state));
        assert_eq!(Resolve.name(), RESOLVE_STEP);
        assert_eq!(Resolve.model(context, &state), Some("opus".to_owned()));
        let mut empty = context;
        empty.resolver_model = "";
        assert_eq!(Resolve.model(empty, &state), None);
    }
}

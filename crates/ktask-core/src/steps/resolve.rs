//! The resolve step: once an attempt has ended `failed` or `failed-unknown`, and the task has
//! not yet used up its `max-attempts`, runs the provider in the resolve role to decide whether
//! a fresh attempt is worth trying.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::run::Attempted;
use crate::steps::implementation::{EarlierAttempt, earlier_attempts};
use crate::steps::{Deps, PipelineState, Step, StepOutcome, run_agent_step, run_one_step};
use crate::{
    AttemptRun, AttemptToken, RESOLVE_STEP, RunContext, RunError, Task, TaskId, TaskStatus,
};

/// Appends one `- attempt N: outcome — reason` line per entry of `attempts` to `prompt`.
fn append_attempts(prompt: &mut String, attempts: &[EarlierAttempt]) {
    for attempt in attempts {
        match &attempt.reason {
            Some(reason) => {
                let _ = writeln!(
                    prompt,
                    "- attempt {}: {} — {reason}",
                    attempt.number, attempt.outcome
                );
            }
            None => {
                let _ = writeln!(prompt, "- attempt {}: {}", attempt.number, attempt.outcome);
            }
        }
    }
}

/// Appends `task`'s title, body and acceptance criteria to `prompt`, headed as a resolve
/// prompt.
fn append_header(prompt: &mut String, task: &Task) {
    let _ = writeln!(prompt, "# Resolve: {}", task.title);
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
}

/// Appends `diff` to `prompt`, fenced under its own heading.
fn append_diff(prompt: &mut String, diff: &str) {
    prompt.push_str("\n## What the task has changed so far\n\n```diff\n");
    prompt.push_str(diff);
    if !diff.is_empty() && !diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
}

/// Appends the exact `report` command, run through `binary_path`, for each possible decision
/// of attempt `token` — `retry` takes an optional `--model <name>`, to hand the task's next
/// attempt to a different model when this one keeps failing a cheaper one.
fn append_reporting(prompt: &mut String, token: &AttemptToken, binary_path: &Path) {
    let binary = binary_path.display();
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         You may change files. When you are done, run exactly one of these, with the decision \
         that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} retry [--model <name>]\n\
         \x20\x20\x20\x20{binary} report --token {token} stop --reason \"<why>\"\n"
    );
}

/// The prompt for the resolve step of attempt `token` of `task`: its title, body and
/// acceptance criteria; every attempt so far, `earlier` then this one's own `current_status`
/// and `current_reason`, each with its own outcome and reason; `diff`, everything the task has
/// changed since its first attempt began; and the exact `report` command, run through
/// `binary_path`, to run for each possible decision.
#[must_use]
pub(crate) fn build_resolve_prompt(
    task: &Task,
    token: &AttemptToken,
    binary_path: &Path,
    earlier: &[EarlierAttempt],
    current_status: TaskStatus,
    current_reason: Option<&str>,
    diff: &str,
) -> String {
    let mut prompt = String::new();
    append_header(&mut prompt, task);
    prompt.push_str("\n## Every attempt so far\n\n");
    append_attempts(&mut prompt, earlier);
    append_attempts(
        &mut prompt,
        &[EarlierAttempt {
            number: token.number,
            outcome: current_status.as_str().to_owned(),
            reason: current_reason.map(str::to_owned),
        }],
    );
    append_diff(&mut prompt, diff);
    append_reporting(&mut prompt, token, binary_path);
    prompt
}

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
        run_agent_step(
            deps,
            context,
            state,
            RESOLVE_STEP,
            model.as_deref(),
            &prompt,
        )
    }
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
    if retried {
        let requested_model = crate::attempt::last_retry_model(deps.journal, task.id, number)?;
        return retry_the_task(deps, context, task, steps, requested_model);
    }
    Ok(Attempted {
        id: task.id,
        status: end_status,
        reason: end_reason,
    })
}

/// Sends `task` back to `pending`, the resolver's own `retry` decision, and begins its next
/// attempt at the implementation step at once — with `requested_model`, when the resolver named
/// one — never the sync or health-check gates, which run only once, ahead of a task's very
/// first attempt.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
fn retry_the_task(
    deps: Deps<'_>,
    context: RunContext<'_>,
    task: &Task,
    steps: &[Box<dyn Step>],
    requested_model: Option<String>,
) -> Result<Attempted, RunError> {
    retry_for_resolver(deps, task.id)?;
    super::run_one_attempt(deps, context, task, &[], steps, requested_model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::at;
    use crate::{TaskId, TaskKind};

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
            resolver_model: "opus",
        };
        let state = PipelineState {
            task: &task(),
            token: &AttemptToken::new("proj", TaskId(7), 1),
            start_commit: None,
            committed: None,
            exit_code: None,
            failure: None,
            requested_model: None,
        };
        assert!(Resolve.enabled(context, &state));
        assert_eq!(Resolve.name(), RESOLVE_STEP);
        assert_eq!(Resolve.model(context, &state), Some("opus".to_owned()));
        let mut empty = context;
        empty.resolver_model = "";
        assert_eq!(Resolve.model(empty, &state), None);
    }
}

//! Turns what running a provider produced into a [`StepOutcome`] — the one piece shared by the
//! implementation, review and test steps, none of which otherwise names the others.

use std::time::Duration;

use crate::steps::{Deps, INTERRUPTED, PipelineState, StepOutcome};
use crate::{
    AttemptToken, Exit, Journal, Outcome, Output, ProviderRunError, RunContext, RunError, StepCall,
    Task, TaskStatus, run_provider,
};

/// What a step that ran a provider ended at: its exit code (`None` when the provider could not
/// be run at all, or was killed), the resulting status, the reason when it is not `done`, and
/// the fine-grained outcome the agent itself reported, when it reported anything.
struct AgentOutcome {
    exit_code: Option<i32>,
    status: TaskStatus,
    reason: Option<String>,
    reported: Option<Outcome>,
}

impl AgentOutcome {
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

/// `output`'s own exit code, or the [`AgentOutcome`] to end the step with when it never
/// produced one: the provider was killed past its time limit, or interrupted.
fn exit_code_or_unreported(output: &Output) -> Result<i32, AgentOutcome> {
    match output.exit {
        Exit::Code(code) => Ok(code),
        Exit::Killed => Err(AgentOutcome::unreported(
            "the provider ran past its time limit and was killed".to_owned(),
        )),
        Exit::Interrupted => Err(AgentOutcome::unreported(INTERRUPTED.to_owned())),
    }
}

/// The status and reason a step's own `report`, when it made one, ends at; `exit_code` names
/// what the provider itself exited at, for the one case there is no report at all.
fn status_and_reason(
    report: Option<(Outcome, Option<String>)>,
    exit_code: i32,
) -> (TaskStatus, Option<String>) {
    match report {
        Some((Outcome::Done | Outcome::Approved | Outcome::Accepted, _)) => {
            (TaskStatus::Done, None)
        }
        Some((
            Outcome::Failed
            | Outcome::TooLarge
            | Outcome::ChangesRequested
            | Outcome::Rejected
            | Outcome::Stop,
            reason,
        )) => (TaskStatus::Failed, reason),
        Some((Outcome::NeedsInput, reason)) => (TaskStatus::Blocked, reason),
        Some((Outcome::Retry, reason)) => (TaskStatus::Done, reason),
        None => (
            TaskStatus::FailedUnknown,
            Some(format!(
                "the provider exited with code {exit_code} and reported nothing"
            )),
        ),
    }
}

/// What running a provider for step `step` of attempt `token` of `task` ended at, given what
/// running it produced.
fn agent_outcome(
    journal: &dyn Journal,
    task: &Task,
    token: &AttemptToken,
    step: &str,
    result: Result<Output, ProviderRunError>,
) -> Result<AgentOutcome, RunError> {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return Ok(AgentOutcome::unreported(format!(
                "the provider could not run: {error}"
            )));
        }
    };
    let exit_code = match exit_code_or_unreported(&output) {
        Ok(code) => code,
        Err(outcome) => return Ok(outcome),
    };
    let report = crate::attempt::report_of_step(journal, task.id, token.number, step)?;
    let reported = report.as_ref().map(|(outcome, _)| *outcome);
    let (status, reason) = status_and_reason(report, exit_code);
    Ok(AgentOutcome {
        exit_code: Some(exit_code),
        status,
        reason,
        reported,
    })
}

/// `outcome`, timed at `duration`, as the [`StepOutcome`] it ended at: `Passed` for `done`,
/// `Ended` for anything else.
fn to_step_outcome(duration: Duration, outcome: AgentOutcome) -> StepOutcome {
    if outcome.status == TaskStatus::Done {
        StepOutcome::Passed {
            duration,
            exit_code: outcome.exit_code,
            reason: None,
            reported: outcome.reported,
        }
    } else {
        StepOutcome::Ended {
            duration,
            exit_code: outcome.exit_code,
            status: outcome.status,
            reason: outcome.reason,
            reported: outcome.reported,
        }
    }
}

/// Runs `prompt` through `deps`'s provider for `state`'s attempt's step `step`, with `model` —
/// the model this step runs with, when it has one, passed to the provider alongside the token,
/// attempt number and step name — timing it, and turns what came back into a [`StepOutcome`].
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn run_agent_step(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &mut PipelineState<'_>,
    step: &'static str,
    model: Option<&str>,
    prompt: &str,
) -> Result<StepOutcome, RunError> {
    let started = deps.clock.now();
    let result = run_provider(
        deps.commands,
        deps.provider,
        prompt,
        StepCall {
            token: &state.token.to_string(),
            attempt: state.token.number,
            step,
            model,
        },
        context.project_dir,
        context.attempt_timeout,
    );
    let duration = deps.clock.now().duration_since(started).unwrap_or_default();
    let outcome = agent_outcome(deps.journal, state.task, state.token, step, result)?;
    state.exit_code = outcome.exit_code;
    Ok(to_step_outcome(duration, outcome))
}

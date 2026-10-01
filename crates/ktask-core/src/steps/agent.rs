//! Turns what running a provider produced into a [`StepOutcome`] — the one piece shared by the
//! implementation, review and test steps, none of which otherwise names the others.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::steps::{Deps, INTERRUPTED, PipelineState, StepOutcome};
use crate::{
    AttemptToken, Exit, IMPLEMENTATION, Journal, Outcome, Output, ProviderRunError, Resume,
    RunContext, RunError, StepCall, Task, TaskStatus, run_provider,
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
        Some((Outcome::Skip, reason)) => (TaskStatus::Skipped, reason),
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

/// Where the transcript of session `session` lives, under `sessions_dir` — the tool's own
/// state directory, never the project's working tree.
fn session_transcript_path(sessions_dir: &Path, session: &str) -> PathBuf {
    sessions_dir.join(format!("{session}.log"))
}

/// The session and transcript path the implementation step of `state`'s attempt is told to
/// resume, when `state.requested_session` names one and `step` is the implementation step.
/// `None` for every other step, and for an attempt with nothing to resume.
fn requested_resume(
    step: &str,
    context: RunContext<'_>,
    state: &PipelineState<'_>,
) -> Option<(String, PathBuf)> {
    let session = (step == IMPLEMENTATION).then(|| state.requested_session.clone())??;
    let transcript_path = session_transcript_path(context.sessions_dir, &session);
    Some((session, transcript_path))
}

/// Records `output`'s own prompt and standard output under `session`'s own transcript, and
/// the session itself against this attempt — the implementation step's own bookkeeping, so a
/// later `retry --same-session` has something to resume and something to read back.
fn record_session(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &PipelineState<'_>,
    prompt: &str,
    output: &Output,
    session: &str,
) -> Result<(), RunError> {
    let path = session_transcript_path(context.sessions_dir, session);
    let mut transcript = format!("=== prompt ===\n{prompt}\n\n=== output ===\n");
    transcript.push_str(&String::from_utf8_lossy(&output.stdout));
    if !transcript.ends_with('\n') {
        transcript.push('\n');
    }
    deps.session_log
        .append(&path, transcript.as_bytes())
        .map_err(|error| RunError::Other(error.to_string()))?;
    crate::attempt::record_session(
        deps.journal,
        deps.clock,
        state.task.id,
        state.token.number,
        session,
    )?;
    Ok(())
}

/// For the implementation step only, when `result` ran and the provider's own `read_session`
/// reports a session for it: records it, with its own transcript, against `state`'s attempt.
fn maybe_record_session(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &PipelineState<'_>,
    step: &str,
    prompt: &str,
    result: &Result<Output, ProviderRunError>,
) -> Result<(), RunError> {
    if step != IMPLEMENTATION {
        return Ok(());
    }
    let Ok(output) = result else {
        return Ok(());
    };
    let Some(session) = (deps.provider.read_session)(output) else {
        return Ok(());
    };
    record_session(deps, context, state, prompt, output, &session)
}

/// Runs `prompt` through `deps`'s provider for `state`'s attempt's step `step`, with `model` —
/// the model this step runs with, when it has one, passed to the provider alongside the token,
/// attempt number and step name — timing it, and turns what came back into a [`StepOutcome`].
/// For the implementation step only: when `state.requested_session` names one, the provider is
/// told to resume it; whatever session the provider's own `read_session` reads back from what
/// it produced — `None` when it reported none at all — is recorded against the attempt, with
/// its own transcript kept under `context.sessions_dir`.
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
    let requested = requested_resume(step, context, state);
    let resume = requested.as_ref().map(|(session, transcript_path)| Resume {
        session,
        transcript_path,
    });
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
            resume,
        },
        context.project_dir,
        context.attempt_timeout,
    );
    let duration = deps.clock.now().duration_since(started).unwrap_or_default();
    maybe_record_session(deps, context, state, step, prompt, &result)?;
    let outcome = agent_outcome(deps.journal, state.task, state.token, step, result)?;
    state.exit_code = outcome.exit_code;
    Ok(to_step_outcome(duration, outcome))
}

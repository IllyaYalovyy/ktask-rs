//! Turns what running a provider produced into a [`StepOutcome`] — the one piece shared by the
//! implementation, review and test steps, none of which otherwise names the others.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::route::{Killed, Signals, output_tail};
use crate::steps::{Deps, PipelineState, StepOutcome};
use crate::{
    AttemptToken, Exit, IMPLEMENTATION, Output, ProviderRunError, Resume, RunContext, RunError,
    StepCall, TaskStatus, run_provider,
};

mod outcome;

use outcome::{AgentOutcome, agent_outcome};

/// `outcome`, timed at `duration`, as the [`StepOutcome`] it ended at: `Passed` for `done`,
/// `Ended` for anything else.
fn to_step_outcome(duration: Duration, outcome: AgentOutcome, signals: Signals) -> StepOutcome {
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
            signals,
        }
    }
}

/// Where the transcript of session `session` lives, under `sessions_dir` — the tool's own
/// state directory, never the project's working tree.
fn session_transcript_path(sessions_dir: &Path, session: &str) -> PathBuf {
    sessions_dir.join(format!("{session}.log"))
}

/// Where the whole prompt handed to step `step` of attempt `token` is written, under
/// `sessions_dir` — the tool's own state directory, never the project's working tree — a
/// scratch file for this one call, removed once it has run.
fn prompt_scratch_path(sessions_dir: &Path, token: &AttemptToken, step: &str) -> PathBuf {
    sessions_dir.join(format!("{}-{}-{step}.prompt", token.task, token.number))
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
    let Some(session) = (deps.provider_for(step).read_session)(output) else {
        return Ok(());
    };
    record_session(deps, context, state, prompt, output, &session)
}

/// Writes `prompt` to its own scratch file under `context.sessions_dir`, runs it through
/// `deps`'s provider for step `step` of `state`'s attempt, with `model` and `resume` passed
/// alongside the token, attempt number and step name, and the scratch file's own path — so a
/// provider whose own script or command line only ever sees part of its prompt can still read
/// everything it said — then removes the scratch file again, whatever running it produced.
/// Returns how long it took and what it produced.
///
/// # Errors
///
/// Fails when `prompt`'s scratch file cannot be written to or removed from
/// `context.sessions_dir`.
fn run_prompt(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &PipelineState<'_>,
    step: &'static str,
    model: Option<&str>,
    prompt: &str,
    resume: Option<Resume<'_>>,
) -> Result<(Duration, Result<Output, ProviderRunError>), RunError> {
    let prompt_path = prompt_scratch_path(context.sessions_dir, state.token, step);
    deps.session_log
        .write_prompt(&prompt_path, prompt)
        .map_err(|error| RunError::Other(error.to_string()))?;
    let started = deps.clock.now();
    let result = run_provider(
        deps.commands,
        deps.provider_for(step),
        prompt,
        StepCall {
            token: &state.token.to_string(),
            attempt: state.token.number,
            step,
            model,
            resume,
            prompt_path: &prompt_path,
            project_dir: context.project_dir,
        },
        context.project_dir,
        attempt_limit(context, state),
        Some(&context.step_output_path(state.token, step)),
    );
    let duration = deps.clock.now().duration_since(started).unwrap_or_default();
    deps.session_log
        .remove_prompt(&prompt_path)
        .map_err(|error| RunError::Other(error.to_string()))?;
    Ok((duration, result))
}

/// Runs `prompt` through `deps`'s provider for `state`'s attempt's step `step`, with `model` —
/// the model this step runs with, when it has one — and turns what came back into a
/// [`StepOutcome`]. For the implementation step only: when `state.requested_session` names one,
/// the provider is told to resume it; whatever session the provider's own `read_session` reads
/// back from what it produced — `None` when it reported none at all — is recorded against the
/// attempt, with its own transcript kept under `context.sessions_dir`.
///
/// # Errors
///
/// Fails when the journal cannot be read, or [`run_prompt`] fails.
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
    let (duration, result) = run_prompt(deps, context, state, step, model, prompt, resume)?;
    maybe_record_session(deps, context, state, step, prompt, &result)?;
    let signals = signals_of(deps, context, state, step, &result);
    let outcome = final_agent_outcome(deps, state, step, model, result)?;
    Ok(to_step_outcome(duration, outcome, signals))
}

/// Reads a completed provider call's facts, applies them to the attempt, and turns its output
/// into the final step outcome.
fn final_agent_outcome(
    deps: &Deps<'_>,
    state: &mut PipelineState<'_>,
    step: &str,
    model: Option<&str>,
    result: Result<Output, ProviderRunError>,
) -> Result<AgentOutcome, RunError> {
    let facts = result.as_ref().map_or_else(
        |_| crate::ProviderUsage::default(),
        |output| (deps.provider_for(step).read_usage)(output),
    );
    state.usage = facts.usage;
    state.used_model.clone_from(&facts.model);
    state.limit_warning.clone_from(&facts.limit_warning);
    let mut outcome = agent_outcome(deps.journal, state.task, state.token, step, result)?;
    if let (Some(asked), Some(used)) = (model, facts.model.as_deref())
        && asked != used
    {
        outcome.status = TaskStatus::Failed;
        outcome.reason = Some(format!("asked for {asked}, the provider used {used}"));
    }
    state.exit_code = outcome.exit_code;
    Ok(outcome)
}

/// How long one provider run of `state`'s attempt may last: the project's attempt time limit,
/// plus the minutes a decider's `retry --more-time` added to this attempt.
fn attempt_limit(context: RunContext<'_>, state: &PipelineState<'_>) -> Duration {
    context.attempt_timeout + state.extra_time
}

/// What `result` showed besides its exit code, for the router to read.
fn signals_of(
    deps: &Deps<'_>,
    context: RunContext<'_>,
    state: &PipelineState<'_>,
    step: &str,
    result: &Result<Output, ProviderRunError>,
) -> Signals {
    let Ok(output) = result else {
        return Signals::default();
    };
    let killed = (output.exit == Exit::Killed).then(|| {
        let last_output = deps
            .output
            .last_output_at(state.task.id, state.token.number);
        Killed {
            after: attempt_limit(context, state),
            last_output_ago: last_output
                .map(|at| deps.clock.now().duration_since(at).unwrap_or_default()),
            tail: output_tail(&output.stdout, &output.stderr),
        }
    });
    Signals {
        limit: (deps.provider_for(step).detect_limit)(output),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        killed,
        check_output: None,
    }
}

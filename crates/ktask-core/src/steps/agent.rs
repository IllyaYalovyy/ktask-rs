//! Turns what running a provider produced into a [`StepOutcome`] — the one piece shared by the
//! implementation, review and test steps, none of which otherwise names the others.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::steps::{Deps, INTERRUPTED, PipelineState, StepOutcome};
use crate::{
    AttemptToken, Exit, IMPLEMENTATION, Journal, Outcome, Output, ProviderRunError, Resume,
    RunContext, RunError, StepCall, Task, TaskStatus, run_provider,
};

/// How long an attempt waits before trying again when its provider's own message said its
/// usage limit was hit but named no reset time of its own.
const DEFAULT_LIMIT_BACKOFF: Duration = Duration::from_mins(5);

/// Operating-system error phrases worth keeping verbatim in the reason when the provider never
/// got the chance to report anything of its own — otherwise lost the moment `report` reads
/// back nothing, leaving only the uninformative "exited with code N and reported nothing".
/// [`super::known_cause`] keys its own disk-full and file-slots-full causes on exactly these.
const KNOWN_OS_ERROR_PHRASES: [&str; 2] = ["No space left on device", "Too many open files"];
/// Claude Code errors the mechanical known-cause rules can fix without asking a resolver to
/// rediscover them. Their full text is retained as the step reason when no report was made.
const KNOWN_CLAUDE_ERROR_PHRASES: [&str; 3] =
    ["Invalid API key", "Not logged in", "Invalid settings"];

/// The first of [`KNOWN_OS_ERROR_PHRASES`] found in `output`'s own standard output or standard
/// error, when there is one.
fn known_os_error(output: &Output) -> Option<&'static str> {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    KNOWN_OS_ERROR_PHRASES
        .into_iter()
        .find(|phrase| text.contains(phrase))
}

/// The first Claude authentication or settings error from an unreported provider output.
fn known_claude_error(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .find(|line| {
            KNOWN_CLAUDE_ERROR_PHRASES
                .iter()
                .any(|phrase| line.contains(phrase))
        })
        .map(str::to_owned)
}

/// The concise diagnostic conventional CLI providers place on standard error. The runner has
/// no provider-specific dependency here: any process that writes a line beginning `ERROR:`
/// keeps that observed reason when it exits before reporting.
fn reported_error(output: &Output) -> Option<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find(|line| line.starts_with("ERROR:"))
        .map(str::to_owned)
}

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
/// what the provider itself exited at, and `output` is searched for a known operating-system
/// error phrase to keep, for the one case there is no report at all.
fn status_and_reason(
    report: Option<(Outcome, Option<String>)>,
    exit_code: i32,
    output: &Output,
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
        Some((Outcome::Supersede, reason)) => (TaskStatus::Superseded, reason),
        Some((Outcome::NeedsInput, reason)) => (TaskStatus::Blocked, reason),
        Some((Outcome::Retry, reason)) => (TaskStatus::Done, reason),
        None => {
            let reason = match known_os_error(output) {
                Some(phrase) => format!(
                    "the provider exited with code {exit_code} and reported nothing: {phrase}"
                ),
                None => reported_error(output)
                    .or_else(|| known_claude_error(output))
                    .unwrap_or_else(|| {
                        format!("the provider exited with code {exit_code} and reported nothing")
                    }),
            };
            (TaskStatus::FailedUnknown, Some(reason))
        }
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
    let (status, reason) = status_and_reason(report, exit_code, &output);
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

/// Where the whole prompt handed to step `step` of attempt `token` is written, under
/// `sessions_dir` — the tool's own state directory, never the project's working tree — a
/// scratch file for this one call, removed once it has run.
fn prompt_scratch_path(sessions_dir: &Path, token: &AttemptToken, step: &str) -> PathBuf {
    sessions_dir.join(format!("{}-{}-{step}.prompt", token.task, token.number))
}

fn attempt_output_path(outputs_dir: &Path, token: &AttemptToken) -> PathBuf {
    outputs_dir.join(format!("{}-{}.log", token.task, token.number))
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
        context.attempt_timeout,
        Some(&attempt_output_path(context.outputs_dir, state.token)),
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
    if let Some(until) = limit_wait(deps, step, &result) {
        return Ok(StepOutcome::Waiting { duration, until });
    }
    maybe_record_session(deps, context, state, step, prompt, &result)?;
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
    Ok(to_step_outcome(duration, outcome))
}

/// The time to wait until before running this step again, when `result`'s own output says the
/// provider's usage limit was hit: the message's own reset time, or [`DEFAULT_LIMIT_BACKOFF`]
/// from now when it named none. `None` when the provider could not even be run, or its output
/// says no such thing.
fn limit_wait(
    deps: &Deps<'_>,
    step: &str,
    result: &Result<Output, ProviderRunError>,
) -> Option<SystemTime> {
    let output = result.as_ref().ok()?;
    let signal = (deps.provider_for(step).detect_limit)(output)?;
    Some(
        signal
            .reset_at
            .unwrap_or_else(|| deps.clock.now() + DEFAULT_LIMIT_BACKOFF),
    )
}

//! Classification of completed provider calls into a typed agent-step outcome.

use crate::steps::INTERRUPTED;
use crate::{
    AttemptToken, Exit, Journal, Outcome, Output, ProviderRunError, RunError, Task, TaskStatus,
};

/// Operating-system error phrases worth keeping verbatim in an otherwise unreported failure.
const KNOWN_OS_ERROR_PHRASES: [&str; 2] = ["No space left on device", "Too many open files"];
/// Claude errors the mechanical known-cause rules can resolve without a resolver.
const KNOWN_CLAUDE_ERROR_PHRASES: [&str; 3] =
    ["Invalid API key", "Not logged in", "Invalid settings"];
/// Codex's authentication rejection with no credentials.
const CODEX_AUTHENTICATION_PHRASES: [&str; 2] =
    ["Missing bearer or basic authentication", "401 Unauthorized"];

/// What a step that ran a provider ended at, including its process, task, and reported outcomes.
pub(super) struct AgentOutcome {
    pub(super) exit_code: Option<i32>,
    pub(super) status: TaskStatus,
    pub(super) reason: Option<String>,
    pub(super) reported: Option<Outcome>,
}

impl AgentOutcome {
    /// No report could have been read because the provider itself never completed.
    fn unreported(reason: String) -> Self {
        Self {
            exit_code: None,
            status: TaskStatus::FailedUnknown,
            reason: Some(reason),
            reported: None,
        }
    }
}

/// Classifies one provider result and reads its step report when the provider completed.
pub(super) fn agent_outcome(
    journal: &dyn Journal,
    task: &Task,
    token: &AttemptToken,
    step: &str,
    result: Result<Output, ProviderRunError>,
) -> Result<AgentOutcome, RunError> {
    let output = result.map_err(|error| format!("the provider could not run: {error}"));
    let output = match output {
        Ok(output) => output,
        Err(reason) => return Ok(AgentOutcome::unreported(reason)),
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

/// The process exit code, or a typed outcome for a killed or interrupted provider.
fn exit_code_or_unreported(output: &Output) -> Result<i32, AgentOutcome> {
    match output.exit {
        Exit::Code(code) => Ok(code),
        Exit::Killed => Err(AgentOutcome::unreported(
            "the provider ran past its time limit and was killed".to_owned(),
        )),
        Exit::Interrupted => Err(AgentOutcome::unreported(INTERRUPTED.to_owned())),
    }
}

/// Classifies the report, or preserves the most useful observed error when it made none.
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
        None => (
            TaskStatus::FailedUnknown,
            Some(unreported_reason(output, exit_code)),
        ),
    }
}

/// The useful diagnostic retained for a provider that exited without a task report.
fn unreported_reason(output: &Output, exit_code: i32) -> String {
    known_os_error(output)
        .map(|phrase| {
            format!("the provider exited with code {exit_code} and reported nothing: {phrase}")
        })
        .or_else(|| reported_error(output))
        .or_else(|| known_claude_error(output))
        .or_else(|| known_codex_authentication_error(output))
        .unwrap_or_else(|| {
            format!("the provider exited with code {exit_code} and reported nothing")
        })
}

/// The first known operating-system error in the provider's output.
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

/// The first Claude authentication or settings error in the provider output.
fn known_claude_error(output: &Output) -> Option<String> {
    known_line(output, &KNOWN_CLAUDE_ERROR_PHRASES)
}

/// The first Codex authentication error in the provider output.
fn known_codex_authentication_error(output: &Output) -> Option<String> {
    known_line(output, &CODEX_AUTHENTICATION_PHRASES)
}

/// The first output line containing any one of `phrases`.
fn known_line(output: &Output, phrases: &[&str]) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .find(|line| phrases.iter().any(|phrase| line.contains(phrase)))
        .map(str::to_owned)
}

/// The conventional CLI error line, when the provider wrote one.
fn reported_error(output: &Output) -> Option<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find(|line| line.starts_with("ERROR:"))
        .map(str::to_owned)
}

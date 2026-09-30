//! Rendering a task's status: the current step's own fields, plus every step run so far.

use std::io::Write;

use ktask_core::StatusEntry;
use serde::Serialize;

/// One step of a task's attempt as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StepJson<'a> {
    step: &'a str,
    provider: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    reason: Option<&'a str>,
}

/// One task's attempt as `status --json` shows it: the current — most recent — step's own
/// fields, kept flat here for whatever only cares about that, plus `steps`, every step run so
/// far, in order.
#[derive(Debug, Serialize)]
struct AttemptJson<'a> {
    number: u32,
    step: &'a str,
    provider: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    reason: Option<&'a str>,
    steps: Vec<StepJson<'a>>,
}

/// One task as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StatusJson<'a> {
    id: u64,
    title: &'a str,
    status: &'static str,
    attempt: AttemptJson<'a>,
}

/// `line` as a [`StepJson`].
fn step_json(line: &ktask_core::StepLine) -> StepJson<'_> {
    StepJson {
        step: &line.step,
        provider: line.provider.as_deref(),
        time_spent_seconds: line.time_spent.as_secs(),
        outcome: line.outcome.as_str(),
        reason: line.reason.as_deref(),
    }
}

/// Writes `entries`: for every task that was attempted, one `#ID<TAB>status<TAB>title` line
/// followed by an indented line for its attempt — step, provider, time spent, outcome, and
/// the reason when it did not succeed — or a JSON array with `json`. Nothing is written when
/// `entries` is empty.
pub(crate) fn status(
    entries: &[StatusEntry],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        status_json(entries, out)
    } else {
        status_text(entries, out)
    }
}

/// Writes `entries` as a JSON array.
fn status_json(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    let shown: Vec<_> = entries
        .iter()
        .map(|entry| StatusJson {
            id: entry.task.0,
            title: &entry.title,
            status: ktask_core::displayed_status(entry.status, Some(entry.attempt.outcome)),
            attempt: AttemptJson {
                number: entry.attempt.number,
                step: &entry.attempt.step,
                provider: entry.attempt.provider.as_deref(),
                time_spent_seconds: entry.attempt.time_spent.as_secs(),
                outcome: entry.attempt.outcome.as_str(),
                reason: entry.attempt.reason.as_deref(),
                steps: entry.attempt.steps.iter().map(step_json).collect(),
            },
        })
        .collect();
    serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

/// Writes `entries`: for every task, one `#ID<TAB>status<TAB>title` line, followed by one
/// indented line per step its attempt has run so far, in order — step, provider (`-` for a
/// step the tool ran itself, which names none), time spent, outcome, and the reason when it
/// did not succeed.
fn status_text(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    entries
        .iter()
        .try_for_each(|entry| {
            let status = ktask_core::displayed_status(entry.status, Some(entry.attempt.outcome));
            writeln!(out, "#{}\t{}\t{}", entry.task, status, entry.title)?;
            entry.attempt.steps.iter().try_for_each(|step| {
                let provider = step.provider.as_deref().unwrap_or("-");
                let seconds = step.time_spent.as_secs();
                match &step.reason {
                    Some(reason) => writeln!(
                        out,
                        "\t{}\t{provider}\t{seconds}s\t{}\t{reason}",
                        step.step, step.outcome
                    ),
                    None => writeln!(
                        out,
                        "\t{}\t{provider}\t{seconds}s\t{}",
                        step.step, step.outcome
                    ),
                }
            })
        })
        .map_err(|e: std::io::Error| e.to_string())
}

//! Rendering a task's status: the current step's own fields, plus every step run so far.

use std::io::Write;

use jiff::Timestamp;
use ktask_core::StatusEntry;
use serde::Serialize;

/// One step of a task's attempt as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StepJson<'a> {
    step: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
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
    model: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    reason: Option<&'a str>,
    steps: Vec<StepJson<'a>>,
}

/// The reason and when a task was sealed `done` by hand, as `status --json` shows it.
#[derive(Debug, Serialize)]
struct DoneMarkJson<'a> {
    reason: &'a str,
    at: String,
}

/// One task as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StatusJson<'a> {
    id: u64,
    title: &'a str,
    status: &'static str,
    attempt: AttemptJson<'a>,
    history: Vec<AttemptJson<'a>>,
    done_by_user: Option<DoneMarkJson<'a>>,
}

/// `line` as a [`StepJson`].
fn step_json(line: &ktask_core::StepLine) -> StepJson<'_> {
    StepJson {
        step: &line.step,
        provider: line.provider.as_deref(),
        model: line.model.as_deref(),
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

/// `line` as an [`AttemptJson`].
fn attempt_json(line: &ktask_core::AttemptLine) -> AttemptJson<'_> {
    AttemptJson {
        number: line.number,
        step: &line.step,
        provider: line.provider.as_deref(),
        model: line.model.as_deref(),
        time_spent_seconds: line.time_spent.as_secs(),
        outcome: line.outcome.as_str(),
        reason: line.reason.as_deref(),
        steps: line.steps.iter().map(step_json).collect(),
    }
}

/// `mark`, timestamped, as a [`DoneMarkJson`].
fn done_mark_json(mark: &ktask_core::DoneMark) -> Result<DoneMarkJson<'_>, String> {
    let at = Timestamp::try_from(mark.at).map_err(|e| format!("bad done time: {e}"))?;
    Ok(DoneMarkJson {
        reason: &mark.reason,
        at: at.to_string(),
    })
}

/// Writes `entries` as a JSON array.
fn status_json(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    let shown = entries
        .iter()
        .map(|entry| {
            Ok(StatusJson {
                id: entry.task.0,
                title: &entry.title,
                status: ktask_core::displayed_status(entry.status, Some(entry.attempt.outcome)),
                attempt: attempt_json(&entry.attempt),
                history: entry.history.iter().map(attempt_json).collect(),
                done_by_user: entry
                    .done_by_user
                    .as_ref()
                    .map(done_mark_json)
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

/// Writes one line saying `entry`'s task was sealed done by hand, with the reason and when,
/// when it was.
fn write_done_mark_line(out: &mut impl Write, entry: &StatusEntry) -> Result<(), String> {
    let Some(mark) = &entry.done_by_user else {
        return Ok(());
    };
    let at = Timestamp::try_from(mark.at).map_err(|e| format!("bad done time: {e}"))?;
    writeln!(out, "\tmarked done by the user: {} (at {at})", mark.reason).map_err(|e| e.to_string())
}

/// Writes `entries`: for every task, one `#ID<TAB>status<TAB>title` line — followed, for a
/// task sealed done by hand, by one line naming the reason and when — then one indented line
/// per step its attempt has run so far, in order — step, provider (`-` for a step the tool ran
/// itself, which names none), time spent, outcome, and the reason when it did not succeed.
fn status_text(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    for entry in entries {
        let status = ktask_core::displayed_status(entry.status, Some(entry.attempt.outcome));
        writeln!(out, "#{}\t{}\t{}", entry.task, status, entry.title).map_err(|e| e.to_string())?;
        write_done_mark_line(out, entry)?;
        entry
            .history
            .iter()
            .try_for_each(|attempt| {
                write_step_lines(
                    out,
                    &attempt.steps,
                    &format!("attempt {}: ", attempt.number),
                )
            })
            .map_err(|e: std::io::Error| e.to_string())?;
        write_step_lines(out, &entry.attempt.steps, "").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Writes one indented line per step of `steps`, in order — step (named with `prefix` ahead of
/// it, so an earlier attempt's own steps read apart from the current one's, which carries none)
/// provider (`-` for a step the tool ran itself, which names none), the model, for the resolve
/// step, when the project has set one, time spent, outcome, and the reason when it did not
/// succeed.
fn write_step_lines(
    out: &mut impl Write,
    steps: &[ktask_core::StepLine],
    prefix: &str,
) -> Result<(), std::io::Error> {
    steps.iter().try_for_each(|step| {
        let provider = step.provider.as_deref().unwrap_or("-");
        let seconds = step.time_spent.as_secs();
        let name = match &step.model {
            Some(model) => format!("{}\t{model}", step.step),
            None => step.step.clone(),
        };
        match &step.reason {
            Some(reason) => writeln!(
                out,
                "\t{prefix}{name}\t{provider}\t{seconds}s\t{}\t{reason}",
                step.outcome
            ),
            None => writeln!(
                out,
                "\t{prefix}{name}\t{provider}\t{seconds}s\t{}",
                step.outcome
            ),
        }
    })
}

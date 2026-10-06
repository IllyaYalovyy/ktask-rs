//! Rendering a task's status: the current step's own fields, plus every step run so far.

use std::io::Write;

use jiff::Timestamp;
use ktask_core::StatusEntry;
use ktask_tui::presentation;
use serde::Serialize;

/// How long a step waited, in total, for its provider's usage limit, and when it last resumed,
/// as `status --json` shows it.
#[derive(Debug, Serialize)]
struct LimitWaitJson {
    waited_seconds: u64,
    resumed_at: String,
}

#[derive(Debug, Serialize)]
struct LimitWarningJson<'a> {
    window: &'a str,
    utilization_percent: u8,
}

/// One step of a task's attempt as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StepJson<'a> {
    step: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    session: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    reason: Option<String>,
    limit_wait: Option<LimitWaitJson>,
    limit_warning: Option<LimitWarningJson<'a>>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<String>,
}

/// The live output state for a running provider attempt as `status --json` shows it.
#[derive(Debug, Serialize)]
struct OutputActivityJson {
    last_output_seconds_ago: Option<u64>,
    silent_for_seconds: u64,
    active: bool,
    may_be_stuck: bool,
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
    session: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    reason: Option<String>,
    limit_wait: Option<LimitWaitJson>,
    limit_warning: Option<LimitWarningJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_activity: Option<OutputActivityJson>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<String>,
    steps: Vec<StepJson<'a>>,
}

/// `activity` in the stable, machine-readable status form.
fn output_activity_json(activity: &ktask_core::OutputActivity) -> OutputActivityJson {
    OutputActivityJson {
        last_output_seconds_ago: activity
            .last_output_at
            .map(|_| activity.silent_for.as_secs()),
        silent_for_seconds: activity.silent_for.as_secs(),
        active: activity.active,
        may_be_stuck: activity.may_be_stuck,
    }
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

/// `wait`, timestamped, as a [`LimitWaitJson`].
fn limit_wait_json(wait: &ktask_core::LimitWait) -> Result<LimitWaitJson, String> {
    let resumed_at = Timestamp::try_from(wait.resumed_at)
        .map_err(|e| format!("bad limit-wait resume time: {e}"))?;
    Ok(LimitWaitJson {
        waited_seconds: wait.waited.as_secs(),
        resumed_at: resumed_at.to_string(),
    })
}

fn limit_warning_json(warning: &ktask_core::LimitWarning) -> LimitWarningJson<'_> {
    LimitWarningJson {
        window: &warning.window,
        utilization_percent: warning.utilization_percent,
    }
}

/// `line` as a [`StepJson`].
fn step_json(line: &ktask_core::StepLine) -> Result<StepJson<'_>, String> {
    Ok(StepJson {
        step: &line.step,
        provider: line.provider.as_deref(),
        model: line.model.as_deref(),
        session: line.session.as_deref(),
        time_spent_seconds: line.time_spent.as_secs(),
        outcome: presentation::outcome(line.outcome),
        reason: presentation::reason_for(line.reason.as_deref(), line.waiting),
        limit_wait: line.limit_wait.as_ref().map(limit_wait_json).transpose()?,
        limit_warning: line.limit_warning.as_ref().map(limit_warning_json),
        input_tokens: line.usage.input_tokens,
        output_tokens: line.usage.output_tokens,
        cost_usd: cost_usd(line.usage.cost_microusd),
    })
}

fn cost_usd(microusd: Option<u64>) -> Option<String> {
    microusd.map(|value| format!("{}.{:06}", value / 1_000_000, value % 1_000_000))
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
fn attempt_json(line: &ktask_core::AttemptLine) -> Result<AttemptJson<'_>, String> {
    Ok(AttemptJson {
        number: line.number,
        step: &line.step,
        provider: line.provider.as_deref(),
        model: line.model.as_deref(),
        session: line.session.as_deref(),
        time_spent_seconds: line.time_spent.as_secs(),
        outcome: presentation::outcome(line.outcome),
        reason: presentation::reason_for(line.reason.as_deref(), line.waiting),
        limit_wait: line.limit_wait.as_ref().map(limit_wait_json).transpose()?,
        limit_warning: line.limit_warning.as_ref().map(limit_warning_json),
        output_activity: line.output_activity.as_ref().map(output_activity_json),
        input_tokens: line.usage.input_tokens,
        output_tokens: line.usage.output_tokens,
        cost_usd: cost_usd(line.usage.cost_microusd),
        steps: line
            .steps
            .iter()
            .map(step_json)
            .collect::<Result<Vec<_>, String>>()?,
    })
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
                status: presentation::task_status(entry.status, Some(entry.attempt.outcome)),
                attempt: attempt_json(&entry.attempt)?,
                history: entry
                    .history
                    .iter()
                    .map(attempt_json)
                    .collect::<Result<Vec<_>, String>>()?,
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
    Timestamp::try_from(mark.at).map_err(|e| format!("bad done time: {e}"))?;
    writeln!(out, "\t{}", presentation::done_mark_text(mark)).map_err(|e| e.to_string())
}

/// Writes `entries`: for every task, one `#ID<TAB>status<TAB>title` line — followed, for a
/// task sealed done by hand, by one line naming the reason and when — then one indented line
/// per step its attempt has run so far, in order — step, provider (`-` for a step the tool ran
/// itself, which names none), time spent, outcome, and the reason when it did not succeed.
fn status_text(entries: &[StatusEntry], out: &mut impl Write) -> Result<(), String> {
    for entry in entries {
        let status = presentation::task_status(entry.status, Some(entry.attempt.outcome));
        writeln!(
            out,
            "#{}\t{}\t{}\t{}",
            entry.task,
            status,
            entry.title,
            presentation::usage_text(entry.attempt.usage)
        )
        .map_err(|e| e.to_string())?;
        write_done_mark_line(out, entry)?;
        entry
            .history
            .iter()
            .try_for_each(|attempt| {
                write_step_lines(
                    out,
                    &attempt.steps,
                    &presentation::attempt_label(attempt.number),
                    None,
                )
            })
            .map_err(|e: std::io::Error| e.to_string())?;
        write_step_lines(
            out,
            &entry.attempt.steps,
            &presentation::attempt_label(entry.attempt.number),
            entry.attempt.output_activity.as_ref(),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `wait`, as the trailing tab field a step's own text line carries it with, when it waited for
/// its provider's usage limit at least once before it ended: how long, in total, and when it
/// last resumed.
fn limit_wait_suffix(wait: Option<&ktask_core::LimitWait>) -> String {
    wait.map_or_else(String::new, |wait| {
        format!("\t{}", presentation::limit_wait_text(wait))
    })
}

fn limit_warning_suffix(warning: Option<&ktask_core::LimitWarning>) -> String {
    warning.map_or_else(String::new, |warning| {
        format!("\t{}", presentation::limit_warning_text(warning))
    })
}

/// The optional session as its tab-separated status field.
fn session_field(session: Option<&str>) -> String {
    let session = presentation::session_suffix(session);
    if session.is_empty() {
        String::new()
    } else {
        format!("\t{session}")
    }
}

/// Writes one indented line per step of `steps`, in order — step (named with `prefix` ahead of
/// it, so every step line says which attempt it belongs to, the current attempt included)
/// provider (`-` for a step the tool ran itself, which names none), the model, for the resolve
/// step, when the project has set one, time spent, outcome, the reason when it did not succeed,
/// and, when it waited at least once for its provider's usage limit before it ended, how long
/// and when it last resumed.
fn write_step_lines(
    out: &mut impl Write,
    steps: &[ktask_core::StepLine],
    prefix: &str,
    activity: Option<&ktask_core::OutputActivity>,
) -> Result<(), std::io::Error> {
    steps.iter().enumerate().try_for_each(|(index, step)| {
        let provider = step.provider.as_deref().unwrap_or("-");
        let seconds = step.time_spent.as_secs();
        let name = match &step.model {
            Some(model) => format!("{}\t{model}", step.step),
            None => step.step.clone(),
        };
        let session = session_field(step.session.as_deref());
        let limit_wait = limit_wait_suffix(step.limit_wait.as_ref());
        let limit_warning = limit_warning_suffix(step.limit_warning.as_ref());
        let usage = presentation::step_usage_text(step)
            .map(|usage| format!("\t{usage}"))
            .unwrap_or_default();
        let activity = activity_suffix(index + 1 == steps.len(), activity);
        match presentation::reason(step) {
            Some(reason) => writeln!(
                out,
                "\t{prefix}{name}\t{provider}\t{seconds}s\t{}\t{reason}{session}{limit_wait}{limit_warning}{usage}{activity}",
                presentation::outcome(step.outcome)
            ),
            None => writeln!(
                out,
                "\t{prefix}{name}\t{provider}\t{seconds}s\t{}{session}{limit_wait}{limit_warning}{usage}{activity}",
                presentation::outcome(step.outcome)
            ),
        }
    })
}

/// The final step alone carries the live provider-output activity field.
fn activity_suffix(current: bool, activity: Option<&ktask_core::OutputActivity>) -> String {
    current
        .then_some(activity)
        .flatten()
        .map_or_else(String::new, |value| {
            let text = presentation::activity(value);
            format!("\t{} {}", text.indicator, text.message)
        })
}

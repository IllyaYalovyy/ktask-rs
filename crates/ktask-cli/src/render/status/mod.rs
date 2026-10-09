//! Rendering `status`: the run band first, then a task's own fields, plus every step run so
//! far.

use std::io::Write;

use jiff::Timestamp;
use ktask_core::{RunBand, StatusEntry};
use ktask_tui::presentation;

mod json;

pub(super) use json::{
    AttemptJson, DoneMarkJson, OutputActivityJson, attempt_json, done_mark_json,
    output_activity_json,
};

/// Writes the run band — what is running, where it most recently stopped and why, or that the
/// queue is idle — as `status`'s own first line, then `entries`: for every task that was
/// attempted, one `#ID<TAB>status<TAB>title` line followed by an indented line for its attempt
/// — step, provider, time spent, outcome, and the reason when it did not succeed — or, with
/// `json`, a JSON object with the band at `run` and the tasks at `tasks`.
pub(crate) fn status(
    band: &RunBand,
    entries: &[StatusEntry],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        json::status_json(band, entries, out)
    } else {
        status_text(band, entries, out)
    }
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
fn status_text(
    band: &RunBand,
    entries: &[StatusEntry],
    out: &mut impl Write,
) -> Result<(), String> {
    writeln!(out, "{}", presentation::run_band_text(band)).map_err(|e| e.to_string())?;
    for entry in entries {
        let status = presentation::task_status(entry.status, Some(entry.attempt.outcome));
        writeln!(
            out,
            "#{}\t{}\t{}\t{}",
            entry.task,
            status,
            entry.title,
            presentation::usage_text(entry.total_usage())
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
        let is_last = index + 1 == steps.len();
        writeln!(out, "\t{}", step_line_text(step, prefix, is_last, activity))?;
        presentation::finding_lines(&step.findings)
            .into_iter()
            .try_for_each(|line| writeln!(out, "\t{line}"))
    })
}

/// One step's own tab-separated fields, as [`write_step_lines`] writes them — pulled out of it
/// so it stays within the workspace's function-length limit.
fn step_line_text(
    step: &ktask_core::StepLine,
    prefix: &str,
    is_last: bool,
    activity: Option<&ktask_core::OutputActivity>,
) -> String {
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
    let routed = routed_field(step);
    let more_time = step.more_time.map_or_else(String::new, |more_time| {
        format!("\t{}", presentation::more_time_text(more_time))
    });
    let activity = activity_suffix(is_last, activity);
    let outcome = presentation::outcome(step.outcome);
    match presentation::reason(step) {
        Some(reason) => format!(
            "{prefix}{name}\t{provider}\t{seconds}s\t{outcome}{routed}\t{reason}{more_time}{session}{limit_wait}{limit_warning}{usage}{activity}"
        ),
        None => format!(
            "{prefix}{name}\t{provider}\t{seconds}s\t{outcome}{routed}{more_time}{session}{limit_wait}{limit_warning}{usage}{activity}"
        ),
    }
}

/// What the router decided about `step`, as the tab field a status line carries right after
/// its outcome and before its reason — the first thing to know about a failure — when it has
/// one.
fn routed_field(step: &ktask_core::StepLine) -> String {
    step.routed.map_or_else(String::new, |routed| {
        format!("\t{}", presentation::routed_text(routed))
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

//! `status --json`'s own shapes and how they are built from the typed facts `status` returns
//! — pulled out of [`super`] so that file stays within the workspace's file-length limit.
//! `show --json` and the run band's own `status --json` shape embed some of these same shapes,
//! so the three never drift apart.

use std::io::Write;

use jiff::Timestamp;
use ktask_core::{RunBand, StatusEntry};
use ktask_tui::presentation;
use serde::Serialize;

use super::super::band::{RunBandJson, run_band_json};

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

/// One reviewer's finding as `status --json` shows it.
#[derive(Debug, Serialize)]
struct FindingJson<'a> {
    location: &'a str,
    problem: &'a str,
    fix: &'a str,
    scope: &'static str,
}

/// `findings` as `status --json` shows them.
fn findings_json(findings: &[ktask_core::Finding]) -> Vec<FindingJson<'_>> {
    findings
        .iter()
        .map(|finding| FindingJson {
            location: &finding.location,
            problem: &finding.problem,
            fix: &finding.fix,
            scope: finding.scope.as_str(),
        })
        .collect()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    routed: Option<String>,
    reason: Option<String>,
    findings: Vec<FindingJson<'a>>,
    limit_wait: Option<LimitWaitJson>,
    limit_warning: Option<LimitWarningJson<'a>>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<String>,
}

/// The live output state for a running provider attempt as `status --json` shows it.
/// `pub(super)`: the run band's own `status --json` shape embeds this same shape.
#[derive(Debug, Serialize)]
pub(in crate::render) struct OutputActivityJson {
    last_output_seconds_ago: Option<u64>,
    silent_for_seconds: u64,
    active: bool,
    may_be_stuck: bool,
}

/// One task's attempt as `status --json` shows it: the current — most recent — step's own
/// fields, kept flat here for whatever only cares about that, plus `steps`, every step run so
/// far, in order. `pub(super)`: `show --json` embeds this same shape.
#[derive(Debug, Serialize)]
pub(in crate::render) struct AttemptJson<'a> {
    number: u32,
    step: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    session: Option<&'a str>,
    time_spent_seconds: u64,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    routed: Option<String>,
    reason: Option<String>,
    findings: Vec<FindingJson<'a>>,
    limit_wait: Option<LimitWaitJson>,
    limit_warning: Option<LimitWarningJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_activity: Option<OutputActivityJson>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<String>,
    steps: Vec<StepJson<'a>>,
}

/// `activity` in the stable, machine-readable status form. `pub(super)`: also used to build
/// the run band's own `status --json` shape.
pub(in crate::render) fn output_activity_json(
    activity: &ktask_core::OutputActivity,
) -> OutputActivityJson {
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
/// `pub(super)`: `show --json` embeds this same shape.
#[derive(Debug, Serialize)]
pub(in crate::render) struct DoneMarkJson<'a> {
    reason: &'a str,
    at: String,
}

/// One task as `status --json` shows it.
#[derive(Debug, Serialize)]
struct StatusJson<'a> {
    id: u64,
    title: &'a str,
    status: &'static str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<String>,
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
        routed: line.routed.map(presentation::routed_label),
        reason: presentation::reason_for(line.reason.as_deref(), line.waiting),
        findings: findings_json(&line.findings),
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

/// `line` as an [`AttemptJson`]. `pub(super)`: also used to build `show --json`'s object, so
/// the two commands' attempt shapes never drift apart.
pub(in crate::render) fn attempt_json(
    line: &ktask_core::AttemptLine,
) -> Result<AttemptJson<'_>, String> {
    Ok(AttemptJson {
        number: line.number,
        step: &line.step,
        provider: line.provider.as_deref(),
        model: line.model.as_deref(),
        session: line.session.as_deref(),
        time_spent_seconds: line.time_spent.as_secs(),
        outcome: presentation::outcome(line.outcome),
        routed: line.routed.map(presentation::routed_label),
        reason: presentation::reason_for(line.reason.as_deref(), line.waiting),
        findings: findings_json(&line.findings),
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
pub(in crate::render) fn done_mark_json(
    mark: &ktask_core::DoneMark,
) -> Result<DoneMarkJson<'_>, String> {
    let at = Timestamp::try_from(mark.at).map_err(|e| format!("bad done time: {e}"))?;
    Ok(DoneMarkJson {
        reason: &mark.reason,
        at: at.to_string(),
    })
}

/// `status --json`'s whole object: the run band at `run`, every task at `tasks`.
#[derive(Debug, Serialize)]
struct StatusWithBandJson<'a> {
    run: RunBandJson,
    tasks: Vec<StatusJson<'a>>,
}

/// Writes `band` and `entries` as one JSON object.
pub(super) fn status_json(
    band: &RunBand,
    entries: &[StatusEntry],
    out: &mut impl Write,
) -> Result<(), String> {
    let tasks = entries
        .iter()
        .map(|entry| {
            let total = entry.total_usage();
            Ok(StatusJson {
                id: entry.task.0,
                title: &entry.title,
                status: presentation::task_status(entry.status, Some(entry.attempt.outcome)),
                input_tokens: total.input_tokens,
                output_tokens: total.output_tokens,
                cost_usd: cost_usd(total.cost_microusd),
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
    let shown = StatusWithBandJson {
        run: run_band_json(band)?,
        tasks,
    };
    serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

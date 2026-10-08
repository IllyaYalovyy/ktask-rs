//! Rendering the run band as `status --json` shows it: the one JSON object, tagged by `state`,
//! both frontends' typed facts fold to.

use std::time::SystemTime;

use jiff::Timestamp;
use ktask_core::{RunBand, StopKind};
use ktask_tui::presentation;
use serde::Serialize;

use super::status::{OutputActivityJson, output_activity_json};

/// [`ktask_core::RunBand`] as `status --json` shows it: tagged by `state`, carrying `text` —
/// the same line `status`'s own first line and the queue screen show for it — plus the typed
/// facts behind it.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum RunBandJson {
    Running {
        text: String,
        task: u64,
        step: String,
        provider: Option<String>,
        model: Option<String>,
        time_spent_seconds: u64,
        output_activity: Option<OutputActivityJson>,
    },
    Stopped {
        text: String,
        task: u64,
        at: Option<String>,
        #[serde(flatten)]
        cause: StopCauseJson,
    },
    Idle {
        text: String,
        pending: usize,
    },
}

/// [`ktask_core::StopKind`] as `status --json` shows it: tagged by `cause`.
#[derive(Debug, Serialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub(super) enum StopCauseJson {
    Ended {
        status: &'static str,
        reason: Option<String>,
        routed: Option<String>,
    },
    EnvironmentFault {
        step: String,
        reason: String,
    },
    HumanTask,
    Interrupted,
}

/// `at`, when there is one, as the ISO-8601 text `status --json` shows every other timestamp
/// with.
fn band_at_json(at: Option<SystemTime>) -> Result<Option<String>, String> {
    at.map(|at| {
        Timestamp::try_from(at)
            .map(|at| at.to_string())
            .map_err(|e| e.to_string())
    })
    .transpose()
}

/// `kind` as a [`StopCauseJson`].
fn stop_cause_json(kind: &StopKind) -> StopCauseJson {
    match kind {
        StopKind::Ended {
            status,
            reason,
            routed,
        } => StopCauseJson::Ended {
            status: presentation::task_status(*status, None),
            reason: reason.clone(),
            routed: routed.map(presentation::routed_label),
        },
        StopKind::EnvironmentFault { step, reason } => StopCauseJson::EnvironmentFault {
            step: step.clone(),
            reason: reason.clone(),
        },
        StopKind::HumanTask => StopCauseJson::HumanTask,
        StopKind::Interrupted => StopCauseJson::Interrupted,
    }
}

/// `band` as a [`RunBandJson`], carrying the exact words [`presentation::run_band_text`] gives
/// for it.
pub(super) fn run_band_json(band: &RunBand) -> Result<RunBandJson, String> {
    let text = presentation::run_band_text(band);
    Ok(match band {
        RunBand::Running(running) => RunBandJson::Running {
            text,
            task: running.task.0,
            step: running.step.clone(),
            provider: running.provider.clone(),
            model: running.model.clone(),
            time_spent_seconds: running.time_spent.as_secs(),
            output_activity: running.output_activity.as_ref().map(output_activity_json),
        },
        RunBand::Stopped(stopped) => RunBandJson::Stopped {
            text,
            task: stopped.task.0,
            at: band_at_json(stopped.at)?,
            cause: stop_cause_json(&stopped.kind),
        },
        RunBand::Idle { pending } => RunBandJson::Idle {
            text,
            pending: *pending,
        },
    })
}

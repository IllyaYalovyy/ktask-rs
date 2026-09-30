//! Decoding an events row — kind, task, time and JSON payload — back into an [`Event`].

use std::time::{Duration, SystemTime};

use ktask_core::{
    Event, JournalError, Outcome, Placement, TaskDraft, TaskId, TaskKind, TaskStatus,
};
use serde_json::Value;

use super::{
    ATTEMPT_ENDED, ATTEMPT_REPORTED, ATTEMPT_RUNNING, ATTEMPT_STARTED, GATE_FAILED, STEP_ENDED,
    STEP_STARTED, TASK_ADDED, TASK_CANCELLED, failed,
};

fn from_seconds(seconds: i64) -> SystemTime {
    match u64::try_from(seconds) {
        Ok(after_epoch) => SystemTime::UNIX_EPOCH + Duration::from_secs(after_epoch),
        Err(_) => SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
    }
}

/// Builds the "a `{what}` is corrupt" [`JournalError`] a decoder for `kind`, task `task_id`
/// reports.
fn corrupt_event(
    kind: &str,
    task_id: i64,
    what: &str,
    cause: impl std::fmt::Display,
) -> JournalError {
    failed(
        "cannot read the journal's events",
        format!("event {kind} for task {task_id} has a bad {what}: {cause}"),
    )
}

/// The [`Placement`] a `task_added` row's `payload` carries: next to a task when it has a
/// `before` or an `after`, at the end otherwise.
fn decode_placement(payload: &Value) -> Placement {
    if let Some(before) = payload.get("before").and_then(Value::as_u64) {
        Placement::Before(TaskId(before))
    } else if let Some(after) = payload.get("after").and_then(Value::as_u64) {
        Placement::After(TaskId(after))
    } else {
        Placement::End
    }
}

/// The [`TaskDraft`] a `task_added` row's `payload` decodes to.
fn decode_task_draft(
    payload: &Value,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<TaskDraft, JournalError> {
    let field = |name: &str| -> Result<String, JournalError> {
        payload
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| corrupt(name, "missing".to_owned()))
    };
    let strings = |name: &str| -> Result<Vec<String>, JournalError> {
        payload
            .get(name)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .ok_or_else(|| corrupt(name, "missing".to_owned()))
    };
    Ok(TaskDraft {
        title: field("title")?,
        body: field("body")?,
        criteria: strings("criteria")?,
        kind: field("kind")?
            .parse::<TaskKind>()
            .map_err(|e| corrupt("kind", e))?,
        links: strings("links")?,
    })
}

/// The [`Event::TaskAdded`] a `task_added` row's `payload` decodes to.
fn decode_task_added(
    kind: &str,
    task_id: i64,
    id: TaskId,
    at: SystemTime,
    payload: &Value,
) -> Result<Event, JournalError> {
    let corrupt = |what: &str, cause: String| corrupt_event(kind, task_id, what, cause);
    Ok(Event::TaskAdded {
        id,
        draft: decode_task_draft(payload, &corrupt)?,
        placement: decode_placement(payload),
        at,
    })
}

/// The `duration`, `exit_code` and `status` an `attempt_ended` or `step_ended` row's `payload`
/// carries — the fields the two kinds decode identically.
fn decode_duration_exit_status(
    payload: &Value,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<(Duration, Option<i32>, TaskStatus), JournalError> {
    let duration_ms = payload
        .get("duration_ms")
        .and_then(Value::as_i64)
        .ok_or_else(|| corrupt("duration_ms", "missing".to_owned()))?;
    let exit_code = payload
        .get("exit_code")
        .and_then(Value::as_i64)
        .map(|code| i32::try_from(code).unwrap_or(i32::MAX));
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("status", "missing".to_owned()))?
        .parse::<TaskStatus>()
        .map_err(|e| corrupt("status", e))?;
    let duration = Duration::from_millis(
        u64::try_from(duration_ms).map_err(|e| corrupt("duration_ms", e.to_string()))?,
    );
    Ok((duration, exit_code, status))
}

/// The [`Event::AttemptRunning`] a `attempt_running` row's `payload` decodes to.
fn decode_attempt_running(
    payload: &Value,
    id: TaskId,
    number: u32,
    at: SystemTime,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Event, JournalError> {
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt("provider", "missing".to_owned()))?;
    Ok(Event::AttemptRunning {
        id,
        number,
        provider,
        at,
    })
}

/// The [`Event::AttemptReported`] an `attempt_reported` row's `payload` decodes to.
fn decode_attempt_reported(
    payload: &Value,
    id: TaskId,
    number: u32,
    reason: Option<String>,
    at: SystemTime,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Event, JournalError> {
    let outcome = payload
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("outcome", "missing".to_owned()))?
        .parse::<Outcome>()
        .map_err(|e| corrupt("outcome", e))?;
    let step = payload
        .get("step")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Event::AttemptReported {
        id,
        number,
        outcome,
        reason,
        step,
        at,
    })
}

/// The [`Event::AttemptEnded`] an `attempt_ended` row's `payload` decodes to.
fn decode_attempt_ended(
    payload: &Value,
    id: TaskId,
    number: u32,
    reason: Option<String>,
    at: SystemTime,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Event, JournalError> {
    let (duration, exit_code, status) = decode_duration_exit_status(payload, corrupt)?;
    Ok(Event::AttemptEnded {
        id,
        number,
        duration,
        exit_code,
        status,
        reason,
        at,
    })
}

/// The [`Event::StepStarted`] a `step_started` row's `payload` decodes to.
fn decode_step_started(
    payload: &Value,
    id: TaskId,
    number: u32,
    at: SystemTime,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Event, JournalError> {
    let step = payload
        .get("step")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt("step", "missing".to_owned()))?;
    Ok(Event::StepStarted {
        id,
        number,
        step,
        at,
    })
}

/// The [`Event::StepEnded`] a `step_ended` row's `payload` decodes to.
fn decode_step_ended(
    payload: &Value,
    id: TaskId,
    number: u32,
    reason: Option<String>,
    at: SystemTime,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Event, JournalError> {
    let step = payload
        .get("step")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt("step", "missing".to_owned()))?;
    let (duration, exit_code, status) = decode_duration_exit_status(payload, corrupt)?;
    let reported = payload
        .get("reported")
        .and_then(Value::as_str)
        .map(str::parse::<Outcome>)
        .transpose()
        .map_err(|e| corrupt("reported", e))?;
    Ok(Event::StepEnded {
        id,
        number,
        step,
        duration,
        exit_code,
        status,
        reason,
        reported,
        at,
    })
}

/// The [`Event::GateFailed`] a `gate_failed` row's `payload` decodes to.
fn decode_gate_failed(
    kind: &str,
    task_id: i64,
    id: TaskId,
    at: SystemTime,
    payload: &Value,
) -> Result<Event, JournalError> {
    let corrupt = |what: &str, cause: String| corrupt_event(kind, task_id, what, cause);
    let field = |name: &str| -> Result<String, JournalError> {
        payload
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| corrupt(name, "missing".to_owned()))
    };
    Ok(Event::GateFailed {
        id,
        step: field("step")?,
        reason: field("reason")?,
        at,
    })
}

/// The attempt event an `attempt_started`, `attempt_running`, `attempt_reported`,
/// `attempt_ended`, `step_started` or `step_ended` row's `payload` decodes to.
fn decode_attempt_event(
    kind: &str,
    task_id: i64,
    id: TaskId,
    at: SystemTime,
    payload: &Value,
) -> Result<Event, JournalError> {
    let corrupt = |what: &str, cause: String| corrupt_event(kind, task_id, what, cause);
    let number = payload
        .get("number")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| corrupt("number", "missing".to_owned()))?;
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match kind {
        ATTEMPT_STARTED => Ok(Event::AttemptStarted { id, number, at }),
        ATTEMPT_RUNNING => decode_attempt_running(payload, id, number, at, &corrupt),
        ATTEMPT_REPORTED => decode_attempt_reported(payload, id, number, reason, at, &corrupt),
        ATTEMPT_ENDED => decode_attempt_ended(payload, id, number, reason, at, &corrupt),
        STEP_STARTED => decode_step_started(payload, id, number, at, &corrupt),
        STEP_ENDED => decode_step_ended(payload, id, number, reason, at, &corrupt),
        _ => Err(corrupt("kind", kind.to_owned())),
    }
}

/// The event a row of kind `kind`, for `task_id` at `at`, with `payload`, decodes to.
pub(super) fn decode_event(
    kind: &str,
    task_id: i64,
    at: i64,
    payload: &str,
) -> Result<Event, JournalError> {
    let id =
        TaskId(u64::try_from(task_id).map_err(|e| corrupt_event(kind, task_id, "task id", e))?);
    let at = from_seconds(at);
    if kind == TASK_CANCELLED {
        return Ok(Event::TaskCancelled { id, at });
    }
    let payload: Value =
        serde_json::from_str(payload).map_err(|e| corrupt_event(kind, task_id, "payload", e))?;
    if kind == TASK_ADDED {
        decode_task_added(kind, task_id, id, at, &payload)
    } else if kind == GATE_FAILED {
        decode_gate_failed(kind, task_id, id, at, &payload)
    } else {
        decode_attempt_event(kind, task_id, id, at, &payload)
    }
}

//! Decoding an events row — kind, task, time and JSON payload — back into an [`Event`].

use std::time::{Duration, SystemTime};

use ktask_core::{Event, JournalError, Placement, TaskDraft, TaskId, TaskKind};
use serde_json::Value;

use super::{
    ATTEMPT_ENDED, ATTEMPT_REPORTED, ATTEMPT_RUNNING, ATTEMPT_SESSION_RECORDED, ATTEMPT_STARTED,
    ATTEMPT_WAITING, GATE_FAILED, STEP_ENDED, STEP_STARTED, TASK_ACKNOWLEDGED, TASK_ADDED,
    TASK_ANSWERED, TASK_CANCELLED, TASK_DONE_BY_USER, TASK_RETRIED, failed,
};

mod attempt;

use attempt::{
    decode_attempt_ended, decode_attempt_reported, decode_attempt_running,
    decode_attempt_session_recorded, decode_attempt_started, decode_attempt_waiting,
    decode_step_ended, decode_step_started,
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
        provider: optional_string(payload, "provider", corrupt)?,
        model: optional_string(payload, "model", corrupt)?,
    })
}

/// An optional string written by a newer task event. Missing values keep older journals valid.
fn optional_string(
    payload: &Value,
    name: &str,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Option<String>, JournalError> {
    match payload.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| corrupt(name, "not a string".to_owned())),
    }
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

/// The [`Event::TaskAnswered`] a `task_answered` row's `payload` decodes to.
fn decode_task_answered(
    kind: &str,
    task_id: i64,
    id: TaskId,
    at: SystemTime,
    payload: &Value,
) -> Result<Event, JournalError> {
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt_event(kind, task_id, "text", "missing"))?;
    Ok(Event::TaskAnswered { id, text, at })
}

/// The [`Event::TaskDoneByUser`] a `task_done_by_user` row's `payload` decodes to.
fn decode_task_done_by_user(
    kind: &str,
    task_id: i64,
    id: TaskId,
    at: SystemTime,
    payload: &Value,
) -> Result<Event, JournalError> {
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt_event(kind, task_id, "reason", "missing"))?;
    Ok(Event::TaskDoneByUser { id, reason, at })
}

/// The [`Event::TaskAcknowledged`] a `task_acknowledged` row's `payload` decodes to.
fn decode_task_acknowledged(
    kind: &str,
    task_id: i64,
    id: TaskId,
    at: SystemTime,
    payload: &Value,
) -> Result<Event, JournalError> {
    let message = match payload.get("message") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| corrupt_event(kind, task_id, "message", "not a string"))?,
        ),
    };
    Ok(Event::TaskAcknowledged { id, message, at })
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
        ATTEMPT_STARTED => Ok(decode_attempt_started(payload, id, number, at)),
        ATTEMPT_RUNNING => decode_attempt_running(payload, id, number, at, &corrupt),
        ATTEMPT_WAITING => decode_attempt_waiting(payload, id, number, at, &corrupt),
        ATTEMPT_REPORTED => decode_attempt_reported(payload, id, number, reason, at, &corrupt),
        ATTEMPT_SESSION_RECORDED => {
            decode_attempt_session_recorded(payload, id, number, at, &corrupt)
        }
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
    if kind == TASK_RETRIED {
        return Ok(Event::TaskRetried { id, at });
    }
    let payload: Value =
        serde_json::from_str(payload).map_err(|e| corrupt_event(kind, task_id, "payload", e))?;
    if kind == TASK_ADDED {
        decode_task_added(kind, task_id, id, at, &payload)
    } else if kind == GATE_FAILED {
        decode_gate_failed(kind, task_id, id, at, &payload)
    } else if kind == TASK_ANSWERED {
        decode_task_answered(kind, task_id, id, at, &payload)
    } else if kind == TASK_DONE_BY_USER {
        decode_task_done_by_user(kind, task_id, id, at, &payload)
    } else if kind == TASK_ACKNOWLEDGED {
        decode_task_acknowledged(kind, task_id, id, at, &payload)
    } else {
        decode_attempt_event(kind, task_id, id, at, &payload)
    }
}

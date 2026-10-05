//! Decoding the payload of an attempt or step event — [`super::decode_attempt_event`]'s own
//! lowest-level work, pulled out of it so that file stays within the workspace's file-length
//! limit.

use std::time::{Duration, SystemTime};

use ktask_core::{
    Event, JournalError, LimitWait, LimitWarning, Outcome, TaskId, TaskStatus, Usage,
};
use serde_json::Value;

/// The [`Event::AttemptStarted`] an `attempt_started` row's `payload` decodes to.
pub(super) fn decode_attempt_started(
    payload: &Value,
    id: TaskId,
    number: u32,
    at: SystemTime,
) -> Event {
    let start_commit = payload
        .get("start_commit")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Event::AttemptStarted {
        id,
        number,
        start_commit,
        at,
    }
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
pub(super) fn decode_attempt_running(
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

/// `payload`'s own `key`, as a bool, or `false` when it is missing or not one.
fn bool_field(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The [`Event::AttemptWaiting`] an `attempt_waiting` row's `payload` decodes to.
pub(super) fn decode_attempt_waiting(
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
    let until = payload
        .get("until")
        .and_then(Value::as_i64)
        .map(super::from_seconds)
        .ok_or_else(|| corrupt("until", "missing".to_owned()))?;
    Ok(Event::AttemptWaiting {
        id,
        number,
        step,
        until,
        at,
    })
}

/// The [`Event::AttemptReported`] an `attempt_reported` row's `payload` decodes to.
pub(super) fn decode_attempt_reported(
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
    let retry_model = payload
        .get("retry_model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let retry_same_session = bool_field(payload, "retry_same_session");
    let retry_reset_tree = bool_field(payload, "retry_reset_tree");
    Ok(Event::AttemptReported {
        id,
        number,
        outcome,
        reason,
        retry_model,
        retry_same_session,
        retry_reset_tree,
        step,
        at,
    })
}

/// The [`Event::AttemptSessionRecorded`] an `attempt_session_recorded` row's `payload` decodes
/// to.
pub(super) fn decode_attempt_session_recorded(
    payload: &Value,
    id: TaskId,
    number: u32,
    at: SystemTime,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Event, JournalError> {
    let session = payload
        .get("session")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt("session", "missing".to_owned()))?;
    Ok(Event::AttemptSessionRecorded {
        id,
        number,
        session,
        at,
    })
}

/// The [`Event::AttemptEnded`] an `attempt_ended` row's `payload` decodes to.
pub(super) fn decode_attempt_ended(
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
pub(super) fn decode_step_started(
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
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Event::StepStarted {
        id,
        number,
        step,
        provider,
        model,
        at,
    })
}

/// The [`LimitWait`] a `step_ended` row's `payload` carries, when it names one: its step waited
/// for its provider's usage limit at least once before it ended.
fn decode_limit_wait(payload: &Value) -> Option<LimitWait> {
    let waited = payload.get("limit_wait_seconds").and_then(Value::as_u64)?;
    let resumed_at = payload.get("limit_resumed_at").and_then(Value::as_i64)?;
    Some(LimitWait {
        waited: Duration::from_secs(waited),
        resumed_at: super::from_seconds(resumed_at),
    })
}

fn decode_limit_warning(payload: &Value) -> Option<LimitWarning> {
    Some(LimitWarning {
        window: payload.get("limit_warning_window")?.as_str()?.to_owned(),
        utilization_percent: u8::try_from(
            payload.get("limit_warning_utilization_percent")?.as_u64()?,
        )
        .ok()?,
    })
}

fn decode_usage(payload: &Value) -> Usage {
    Usage {
        input_tokens: payload.get("input_tokens").and_then(Value::as_u64),
        output_tokens: payload.get("output_tokens").and_then(Value::as_u64),
        cost_microusd: payload.get("cost_microusd").and_then(Value::as_u64),
    }
}

/// The [`Event::StepEnded`] a `step_ended` row's `payload` decodes to.
pub(super) fn decode_step_ended(
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
        limit_wait: decode_limit_wait(payload),
        limit_warning: decode_limit_warning(payload),
        usage: decode_usage(payload),
        used_model: payload
            .get("used_model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        at,
    })
}

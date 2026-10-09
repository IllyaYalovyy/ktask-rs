//! Decoding the payload of an attempt or step event — [`super::decode_attempt_event`]'s own
//! lowest-level work, pulled out of it so that file stays within the workspace's file-length
//! limit.

use std::time::{Duration, SystemTime};

use ktask_core::{
    Event, Finding, FindingScope, JournalError, LimitWait, LimitWarning, Outcome, Routed, TaskId,
    TaskStatus, Usage, WaitReason,
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

/// The [`WaitReason`] an `attempt_waiting` payload names: a transport retry when it carries
/// one, the provider's usage limit otherwise.
fn decode_wait_reason(
    payload: &Value,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<WaitReason, JournalError> {
    let Some(retry) = payload
        .get("transport_retry")
        .filter(|retry| !retry.is_null())
    else {
        return Ok(WaitReason::UsageLimit);
    };
    let count = |field: &str| {
        retry
            .get(field)
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok())
            .ok_or_else(|| corrupt(field, "missing".to_owned()))
    };
    Ok(WaitReason::TransportRetry {
        failure: count("failure")?,
        limit: count("limit")?,
    })
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
    let reason = decode_wait_reason(payload, corrupt)?;
    Ok(Event::AttemptWaiting {
        id,
        number,
        step,
        until,
        reason,
        at,
    })
}

/// One [`Finding`] a `finding` value of an `attempt_reported` row's `findings` array decodes to.
fn decode_finding(
    value: &Value,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Finding, JournalError> {
    let field = |name: &str| -> Result<String, JournalError> {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| corrupt("findings", format!("missing {name}")))
    };
    let scope = field("scope")?
        .parse::<FindingScope>()
        .map_err(|e| corrupt("findings", e))?;
    Ok(Finding {
        location: field("location")?,
        problem: field("problem")?,
        fix: field("fix")?,
        scope,
    })
}

/// The findings an `attempt_reported` row's `payload` carries; empty when it carries none, or
/// an older journal recorded it before findings existed at all.
fn decode_findings(
    payload: &Value,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Vec<Finding>, JournalError> {
    payload
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| decode_finding(value, corrupt))
        .collect()
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
    let findings = decode_findings(payload, corrupt)?;
    Ok(Event::AttemptReported {
        id,
        number,
        outcome,
        reason,
        findings,
        retry_model: string_field(payload, "retry_model"),
        retry_same_session: bool_field(payload, "retry_same_session"),
        retry_reset_tree: bool_field(payload, "retry_reset_tree"),
        retry_more_time: payload
            .get("retry_more_time")
            .and_then(Value::as_u64)
            .and_then(|minutes| u32::try_from(minutes).ok()),
        step: string_field(payload, "step"),
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

/// The verdict a `step_ended` row's `payload` records, when it records one.
fn decode_routed(
    payload: &Value,
    corrupt: &impl Fn(&str, String) -> JournalError,
) -> Result<Option<Routed>, JournalError> {
    payload
        .get("routed")
        .and_then(Value::as_str)
        .map(|token| Routed::from_token(token).ok_or_else(|| corrupt("routed", token.to_owned())))
        .transpose()
}

fn string_field(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(str::to_owned)
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
    let routed = decode_routed(payload, corrupt)?;
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
        used_model: string_field(payload, "used_model"),
        routed,
        at,
    })
}

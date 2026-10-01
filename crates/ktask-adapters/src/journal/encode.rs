//! Encoding an [`Event`] into the `(kind, task_id, at, payload)` an events row is written
//! with.

use std::time::SystemTime;

use ktask_core::{Event, Outcome, Placement, TaskDraft, TaskId};
use serde_json::Value;

use super::{
    ATTEMPT_ENDED, ATTEMPT_REPORTED, ATTEMPT_RUNNING, ATTEMPT_SESSION_RECORDED, ATTEMPT_STARTED,
    ATTEMPT_WAITING, GATE_FAILED, STEP_ENDED, STEP_STARTED, TASK_ADDED, TASK_ANSWERED,
    TASK_CANCELLED, TASK_DONE_BY_USER, TASK_RETRIED,
};

pub(super) fn to_seconds(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or_else(
        |before| -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
        |after| i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
    )
}

/// The payload [`Event::TaskAdded`] is written with: the draft's fields, and, when it was
/// placed next to another task, which side.
fn task_added_payload(draft: &TaskDraft, placement: Placement) -> String {
    let placed = match placement {
        Placement::End => None,
        Placement::Before(anchor) => Some(("before", anchor.0)),
        Placement::After(anchor) => Some(("after", anchor.0)),
    };
    let payload = serde_json::json!({
        "title": draft.title,
        "body": draft.body,
        "criteria": draft.criteria,
        "kind": draft.kind.as_str(),
        "links": draft.links,
    })
    .as_object()
    .cloned()
    .into_iter()
    .flatten()
    .chain(placed.map(|(place, anchor)| (place.to_owned(), anchor.into())))
    .collect::<serde_json::Map<_, _>>();
    Value::Object(payload).to_string()
}

/// The payload an `attempt_started` row is written with.
fn attempt_started_payload(event: &Event) -> String {
    let Event::AttemptStarted {
        number,
        start_commit,
        ..
    } = event
    else {
        unreachable!("only called for Event::AttemptStarted")
    };
    serde_json::json!({ "number": number, "start_commit": start_commit }).to_string()
}

/// The payload an `attempt_running` row is written with.
fn attempt_running_payload(event: &Event) -> String {
    let Event::AttemptRunning {
        number, provider, ..
    } = event
    else {
        unreachable!("only called for Event::AttemptRunning")
    };
    serde_json::json!({ "number": number, "provider": provider }).to_string()
}

/// The payload an `attempt_waiting` row is written with.
fn attempt_waiting_payload(event: &Event) -> String {
    let Event::AttemptWaiting {
        number,
        step,
        until,
        ..
    } = event
    else {
        unreachable!("only called for Event::AttemptWaiting")
    };
    serde_json::json!({ "number": number, "step": step, "until": to_seconds(*until) }).to_string()
}

/// The payload an `attempt_reported` row is written with.
fn attempt_reported_payload(event: &Event) -> String {
    let Event::AttemptReported {
        number,
        outcome,
        reason,
        retry_model,
        retry_same_session,
        retry_reset_tree,
        step,
        ..
    } = event
    else {
        unreachable!("only called for Event::AttemptReported")
    };
    serde_json::json!({
        "number": number,
        "outcome": outcome.as_str(),
        "reason": reason,
        "retry_model": retry_model,
        "retry_same_session": retry_same_session,
        "retry_reset_tree": retry_reset_tree,
        "step": step,
    })
    .to_string()
}

/// The payload an `attempt_session_recorded` row is written with.
fn attempt_session_recorded_payload(event: &Event) -> String {
    let Event::AttemptSessionRecorded {
        number, session, ..
    } = event
    else {
        unreachable!("only called for Event::AttemptSessionRecorded")
    };
    serde_json::json!({ "number": number, "session": session }).to_string()
}

/// The payload an `attempt_ended` row is written with.
fn attempt_ended_payload(event: &Event) -> String {
    let Event::AttemptEnded {
        number,
        duration,
        exit_code,
        status,
        reason,
        ..
    } = event
    else {
        unreachable!("only called for Event::AttemptEnded")
    };
    serde_json::json!({
        "number": number,
        "duration_ms": i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        "exit_code": exit_code,
        "status": status.as_str(),
        "reason": reason,
    })
    .to_string()
}

/// The payload a `step_started` row is written with.
fn step_started_payload(event: &Event) -> String {
    let Event::StepStarted {
        number,
        step,
        model,
        ..
    } = event
    else {
        unreachable!("only called for Event::StepStarted")
    };
    serde_json::json!({ "number": number, "step": step, "model": model }).to_string()
}

/// The payload a `step_ended` row is written with.
fn step_ended_payload(event: &Event) -> String {
    let Event::StepEnded {
        number,
        step,
        duration,
        exit_code,
        status,
        reason,
        reported,
        ..
    } = event
    else {
        unreachable!("only called for Event::StepEnded")
    };
    serde_json::json!({
        "number": number,
        "step": step,
        "duration_ms": i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        "exit_code": exit_code,
        "status": status.as_str(),
        "reason": reason,
        "reported": reported.map(Outcome::as_str),
    })
    .to_string()
}

/// The payload a `gate_failed` row is written with.
fn gate_failed_payload(event: &Event) -> String {
    let Event::GateFailed { step, reason, .. } = event else {
        unreachable!("only called for Event::GateFailed")
    };
    serde_json::json!({ "step": step, "reason": reason }).to_string()
}

/// The payload a `task_answered` row is written with.
fn task_answered_payload(event: &Event) -> String {
    let Event::TaskAnswered { text, .. } = event else {
        unreachable!("only called for Event::TaskAnswered")
    };
    serde_json::json!({ "text": text }).to_string()
}

/// The payload a `task_done_by_user` row is written with.
fn task_done_by_user_payload(event: &Event) -> String {
    let Event::TaskDoneByUser { reason, .. } = event else {
        unreachable!("only called for Event::TaskDoneByUser")
    };
    serde_json::json!({ "reason": reason }).to_string()
}

/// The task any `event` carries — every kind of event names one.
fn event_task_id(event: &Event) -> TaskId {
    match event {
        Event::TaskAdded { id, .. }
        | Event::TaskCancelled { id, .. }
        | Event::AttemptStarted { id, .. }
        | Event::AttemptRunning { id, .. }
        | Event::AttemptWaiting { id, .. }
        | Event::AttemptReported { id, .. }
        | Event::AttemptSessionRecorded { id, .. }
        | Event::AttemptEnded { id, .. }
        | Event::StepStarted { id, .. }
        | Event::StepEnded { id, .. }
        | Event::GateFailed { id, .. }
        | Event::TaskRetried { id, .. }
        | Event::TaskAnswered { id, .. }
        | Event::TaskDoneByUser { id, .. } => *id,
    }
}

/// The time any `event` carries — every kind of event happened at one.
fn event_at(event: &Event) -> SystemTime {
    match event {
        Event::TaskAdded { at, .. }
        | Event::TaskCancelled { at, .. }
        | Event::AttemptStarted { at, .. }
        | Event::AttemptRunning { at, .. }
        | Event::AttemptWaiting { at, .. }
        | Event::AttemptReported { at, .. }
        | Event::AttemptSessionRecorded { at, .. }
        | Event::AttemptEnded { at, .. }
        | Event::StepStarted { at, .. }
        | Event::StepEnded { at, .. }
        | Event::GateFailed { at, .. }
        | Event::TaskRetried { at, .. }
        | Event::TaskAnswered { at, .. }
        | Event::TaskDoneByUser { at, .. } => *at,
    }
}

/// `event`'s own kind and payload, without the task and time every kind carries alike — the
/// pieces [`encode_event`] adds itself, through [`event_task_id`] and [`event_at`].
fn event_kind_and_payload(event: &Event) -> (&'static str, String) {
    match event {
        Event::TaskAdded {
            draft, placement, ..
        } => (TASK_ADDED, task_added_payload(draft, *placement)),
        Event::TaskCancelled { .. } => (TASK_CANCELLED, "{}".to_owned()),
        Event::AttemptStarted { .. } => (ATTEMPT_STARTED, attempt_started_payload(event)),
        Event::AttemptRunning { .. } => (ATTEMPT_RUNNING, attempt_running_payload(event)),
        Event::AttemptWaiting { .. } => (ATTEMPT_WAITING, attempt_waiting_payload(event)),
        Event::AttemptReported { .. } => (ATTEMPT_REPORTED, attempt_reported_payload(event)),
        Event::AttemptSessionRecorded { .. } => (
            ATTEMPT_SESSION_RECORDED,
            attempt_session_recorded_payload(event),
        ),
        Event::AttemptEnded { .. } => (ATTEMPT_ENDED, attempt_ended_payload(event)),
        Event::StepStarted { .. } => (STEP_STARTED, step_started_payload(event)),
        Event::StepEnded { .. } => (STEP_ENDED, step_ended_payload(event)),
        Event::GateFailed { .. } => (GATE_FAILED, gate_failed_payload(event)),
        Event::TaskRetried { .. } => (TASK_RETRIED, "{}".to_owned()),
        Event::TaskAnswered { .. } => (TASK_ANSWERED, task_answered_payload(event)),
        Event::TaskDoneByUser { .. } => (TASK_DONE_BY_USER, task_done_by_user_payload(event)),
    }
}

/// `event`, as the `(kind, task_id, at, payload)` an events row is written with.
pub(super) fn encode_event(event: &Event) -> (&'static str, i64, i64, String) {
    let (kind, payload) = event_kind_and_payload(event);
    let task_id = i64::try_from(event_task_id(event).0).unwrap_or(i64::MAX);
    (kind, task_id, to_seconds(event_at(event)), payload)
}

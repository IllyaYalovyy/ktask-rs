//! A project's journal in SQLite.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ktask_core::{
    AppendConflict, Event, Journal, JournalError, Outcome, Placement, TaskDraft, TaskId, TaskKind,
    TaskStatus,
};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde_json::Value;

/// How long a writer waits for another process's transaction to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A project's journal, kept in a SQLite database file.
///
/// `events` is the journal proper: one row per event, appended and never changed — the only
/// thing this adapter decides is whether the count it was given still matches the table's;
/// everything about what an event means — positions, numbers, which placements are valid,
/// which attempt is running, how one ends — is `ktask_core::queue_state`'s. `tasks` is a
/// mechanical mirror of every event in `events`, updated in the same transaction as the event
/// that changes it, kept only because tools outside this crate (and its own tests) read a
/// task's current status without folding the whole journal.
#[derive(Debug)]
pub struct SqliteJournal {
    connection: Connection,
}

/// Why `doing` failed, as a [`JournalError`].
fn failed(doing: &str, cause: impl std::fmt::Display) -> JournalError {
    JournalError::new(format!("{doing}: {cause}"))
}

impl SqliteJournal {
    /// Opens the journal database at `path`, creating the file, its directory and its tables
    /// when they do not exist yet.
    ///
    /// # Errors
    ///
    /// Fails when the directory or file cannot be created or is not a usable database.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let doing = format!("cannot open the journal {}", path.display());
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).map_err(|e| {
                failed(
                    &format!("cannot create the state directory {}", directory.display()),
                    e,
                )
            })?;
        }
        let connection = Connection::open(path).map_err(|e| failed(&doing, e))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| failed(&doing, e))?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS events (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     at INTEGER NOT NULL,
                     kind TEXT NOT NULL,
                     task_id INTEGER NOT NULL,
                     payload TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS tasks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     order_key INTEGER NOT NULL,
                     title TEXT NOT NULL,
                     body TEXT NOT NULL,
                     criteria TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     links TEXT NOT NULL,
                     status TEXT NOT NULL,
                     created_at INTEGER NOT NULL,
                     attempt_number INTEGER NOT NULL DEFAULT 0
                 )",
            )
            .map_err(|e| failed(&doing, e))?;
        Ok(Self { connection })
    }
}

/// The kinds of event `events` and `append_events` know.
const TASK_ADDED: &str = "task_added";
const TASK_CANCELLED: &str = "task_cancelled";
const ATTEMPT_STARTED: &str = "attempt_started";
const ATTEMPT_RUNNING: &str = "attempt_running";
const ATTEMPT_REPORTED: &str = "attempt_reported";
const ATTEMPT_ENDED: &str = "attempt_ended";
const STEP_STARTED: &str = "step_started";
const STEP_ENDED: &str = "step_ended";

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

/// The event a row of kind `kind`, for `task_id` at `at`, with `payload`, decodes to.
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
fn decode_event(kind: &str, task_id: i64, at: i64, payload: &str) -> Result<Event, JournalError> {
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
    } else {
        decode_attempt_event(kind, task_id, id, at, &payload)
    }
}

/// The payload an `attempt_started` row is written with.
fn attempt_started_payload(event: &Event) -> String {
    let Event::AttemptStarted { number, .. } = event else {
        unreachable!("only called for Event::AttemptStarted")
    };
    serde_json::json!({ "number": number }).to_string()
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

/// The payload an `attempt_reported` row is written with.
fn attempt_reported_payload(event: &Event) -> String {
    let Event::AttemptReported {
        number,
        outcome,
        reason,
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
        "step": step,
    })
    .to_string()
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
    let Event::StepStarted { number, step, .. } = event else {
        unreachable!("only called for Event::StepStarted")
    };
    serde_json::json!({ "number": number, "step": step }).to_string()
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

/// The task any `event` carries — every kind of event names one.
fn event_task_id(event: &Event) -> TaskId {
    match event {
        Event::TaskAdded { id, .. }
        | Event::TaskCancelled { id, .. }
        | Event::AttemptStarted { id, .. }
        | Event::AttemptRunning { id, .. }
        | Event::AttemptReported { id, .. }
        | Event::AttemptEnded { id, .. }
        | Event::StepStarted { id, .. }
        | Event::StepEnded { id, .. } => *id,
    }
}

/// The time any `event` carries — every kind of event happened at one.
fn event_at(event: &Event) -> SystemTime {
    match event {
        Event::TaskAdded { at, .. }
        | Event::TaskCancelled { at, .. }
        | Event::AttemptStarted { at, .. }
        | Event::AttemptRunning { at, .. }
        | Event::AttemptReported { at, .. }
        | Event::AttemptEnded { at, .. }
        | Event::StepStarted { at, .. }
        | Event::StepEnded { at, .. } => *at,
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
        Event::AttemptReported { .. } => (ATTEMPT_REPORTED, attempt_reported_payload(event)),
        Event::AttemptEnded { .. } => (ATTEMPT_ENDED, attempt_ended_payload(event)),
        Event::StepStarted { .. } => (STEP_STARTED, step_started_payload(event)),
        Event::StepEnded { .. } => (STEP_ENDED, step_ended_payload(event)),
    }
}

/// `event`, as the `(kind, task_id, at, payload)` an events row is written with.
fn encode_event(event: &Event) -> (&'static str, i64, i64, String) {
    let (kind, payload) = event_kind_and_payload(event);
    let task_id = i64::try_from(event_task_id(event).0).unwrap_or(i64::MAX);
    (kind, task_id, to_seconds(event_at(event)), payload)
}

/// Inserts the row [`Event::TaskAdded`] mirrors into the `tasks` cache.
fn mirror_task_added(
    transaction: &Transaction<'_>,
    task_id: i64,
    draft: &TaskDraft,
    at: SystemTime,
) -> Result<(), rusqlite::Error> {
    let criteria = serde_json::json!(draft.criteria).to_string();
    let links = serde_json::json!(draft.links).to_string();
    transaction.execute(
        "INSERT INTO tasks
             (id, order_key, title, body, criteria, kind, links, status, created_at)
         VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            task_id,
            &draft.title,
            &draft.body,
            &criteria,
            draft.kind.as_str(),
            &links,
            TaskStatus::Pending.as_str(),
            to_seconds(at),
        ),
    )?;
    Ok(())
}

/// Mirrors `event` into the `tasks` cache, inside `transaction`: mechanical bookkeeping so a
/// task's current status can be read without folding the whole journal — no rule about the
/// queue or its attempts is decided here, only what `ktask_core::queue_state` already decided.
fn mirror(transaction: &Transaction<'_>, event: &Event) -> Result<(), rusqlite::Error> {
    let task_id = |id: TaskId| i64::try_from(id.0).unwrap_or(i64::MAX);
    match event {
        Event::TaskAdded { id, draft, at, .. } => {
            mirror_task_added(transaction, task_id(*id), draft, *at)?;
        }
        Event::TaskCancelled { id, .. } => {
            transaction.execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                (task_id(*id), TaskStatus::Cancelled.as_str()),
            )?;
        }
        Event::AttemptStarted { id, number, .. } => {
            transaction.execute(
                "UPDATE tasks SET status = ?2, attempt_number = ?3 WHERE id = ?1",
                (task_id(*id), TaskStatus::Running.as_str(), *number),
            )?;
        }
        Event::AttemptRunning { .. }
        | Event::AttemptReported { .. }
        | Event::StepStarted { .. }
        | Event::StepEnded { .. } => {
            // Carries no status change of its own.
        }
        Event::AttemptEnded { id, status, .. } => {
            transaction.execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                (task_id(*id), status.as_str()),
            )?;
        }
    }
    Ok(())
}

fn to_seconds(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or_else(
        |before| -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
        |after| i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
    )
}

fn from_seconds(seconds: i64) -> SystemTime {
    match u64::try_from(seconds) {
        Ok(after_epoch) => SystemTime::UNIX_EPOCH + Duration::from_secs(after_epoch),
        Err(_) => SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
    }
}

impl Journal for SqliteJournal {
    fn events(&self) -> Result<Vec<Event>, JournalError> {
        let doing = "cannot read the journal's events";
        let mut statement = self
            .connection
            .prepare("SELECT at, kind, task_id, payload FROM events ORDER BY seq")
            .map_err(|e| failed(doing, e))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| failed(doing, e))?;
        let mut events = Vec::new();
        for row in rows {
            let (at, kind, task_id, payload) = row.map_err(|e| failed(doing, e))?;
            events.push(decode_event(&kind, task_id, at, &payload)?);
        }
        Ok(events)
    }

    fn append_events(&self, events: &[Event], read: usize) -> Result<(), AppendConflict> {
        let doing = "cannot append to the journal";
        // Immediate: take the write lock first, so that checking how many events the journal
        // holds and appending more cannot interleave with another process doing the same.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let current: i64 = transaction
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .map_err(|e| failed(doing, e))?;
        if usize::try_from(current).unwrap_or(usize::MAX) != read {
            return Err(AppendConflict::Conflict);
        }
        for event in events {
            let (kind, task_id, at, payload) = encode_event(event);
            transaction
                .execute(
                    "INSERT INTO events (at, kind, task_id, payload) VALUES (?1, ?2, ?3, ?4)",
                    (at, kind, task_id, payload),
                )
                .map_err(|e| failed(doing, e))?;
            mirror(&transaction, event).map_err(|e| failed(doing, e))?;
        }
        transaction.commit().map_err(|e| failed(doing, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn draft(title: &str) -> TaskDraft {
        TaskDraft {
            title: title.to_owned(),
            body: "line one\nline \"two\"".to_owned(),
            criteria: vec!["first".to_owned(), "sécond".to_owned()],
            kind: TaskKind::Human,
            links: vec!["github:o/r#1".to_owned(), "https://example.com".to_owned()],
        }
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn open(dir: &TempDir) -> SqliteJournal {
        SqliteJournal::open(&dir.path().join("journal.db")).unwrap()
    }

    /// Appends `event`, as the caller — always `ktask_core`, having already decided it against
    /// the state it folds to — would, and returns it.
    fn append(journal: &SqliteJournal, event: Event) -> Event {
        let read = journal.events().unwrap().len();
        journal
            .append_events(std::slice::from_ref(&event), read)
            .unwrap();
        event
    }

    /// Appends a `task_added` event for a task numbered `id`, titled `title`, at `placement`.
    fn add(journal: &SqliteJournal, id: u64, title: &str, placement: Placement) -> Event {
        append(
            journal,
            Event::TaskAdded {
                id: TaskId(id),
                draft: draft(title),
                placement,
                at: at(1),
            },
        )
    }

    /// Appends a `task_cancelled` event for task `id` at `moment`.
    fn cancel(journal: &SqliteJournal, id: u64, moment: SystemTime) {
        append(
            journal,
            Event::TaskCancelled {
                id: TaskId(id),
                at: moment,
            },
        );
    }

    /// The `tasks` cache row's status and attempt number for task `id`.
    fn cached(journal: &SqliteJournal, id: u64) -> (TaskStatus, i64) {
        journal
            .connection
            .query_row(
                "SELECT status, attempt_number FROM tasks WHERE id = ?1",
                [i64::try_from(id).unwrap_or(i64::MAX)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .map(|(status, number)| (status.parse().unwrap(), number))
            .unwrap()
    }

    #[test]
    fn a_new_journal_is_created_with_its_directory_and_holds_no_events() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("journal.db");
        let journal = SqliteJournal::open(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(journal.events(), Ok(vec![]));
    }

    #[test]
    fn an_appended_event_is_read_back_as_written_after_reopening() {
        let dir = TempDir::new().unwrap();
        let event = add(&open(&dir), 1, "t", Placement::End);
        assert_eq!(
            event,
            Event::TaskAdded {
                id: TaskId(1),
                draft: draft("t"),
                placement: Placement::End,
                at: at(1),
            }
        );
        assert_eq!(open(&dir).events().unwrap(), vec![event]);
    }

    #[test]
    fn events_are_read_back_in_the_order_they_were_appended_across_reopenings() {
        let dir = TempDir::new().unwrap();
        for (index, title) in ["a", "b", "c"].into_iter().enumerate() {
            let event = add(&open(&dir), index as u64 + 1, title, Placement::End);
            let Event::TaskAdded { id, .. } = event else {
                unreachable!("just added")
            };
            assert_eq!(id, TaskId(index as u64 + 1));
        }
        let titles: Vec<_> = open(&dir)
            .events()
            .unwrap()
            .into_iter()
            .map(|event| match event {
                Event::TaskAdded { draft, .. } => draft.title,
                other => unreachable!("only additions were made: {other:?}"),
            })
            .collect();
        assert_eq!(titles, ["a", "b", "c"]);
    }

    #[test]
    fn placement_before_and_after_round_trip_through_the_event() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        add(&journal, 2, "b", Placement::Before(TaskId(1)));
        add(&journal, 3, "c", Placement::After(TaskId(1)));
        let placements: Vec<_> = journal
            .events()
            .unwrap()
            .into_iter()
            .map(|event| match event {
                Event::TaskAdded { placement, .. } => placement,
                other => unreachable!("only additions were made: {other:?}"),
            })
            .collect();
        assert_eq!(
            placements,
            [
                Placement::End,
                Placement::Before(TaskId(1)),
                Placement::After(TaskId(1))
            ]
        );
    }

    #[test]
    fn a_cancelled_event_round_trips_too() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        cancel(&journal, 1, at(900));
        assert_eq!(
            journal.events().unwrap()[1],
            Event::TaskCancelled {
                id: TaskId(1),
                at: at(900),
            }
        );
    }

    #[test]
    fn every_attempt_event_round_trips() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        let started = append(
            &journal,
            Event::AttemptStarted {
                id: TaskId(1),
                number: 1,
                at: at(10),
            },
        );
        let running = append(
            &journal,
            Event::AttemptRunning {
                id: TaskId(1),
                number: 1,
                provider: "echo".to_owned(),
                at: at(11),
            },
        );
        let reported = append(
            &journal,
            Event::AttemptReported {
                id: TaskId(1),
                number: 1,
                outcome: Outcome::Failed,
                reason: Some("it broke".to_owned()),
                step: None,
                at: at(12),
            },
        );
        let ended = append(
            &journal,
            Event::AttemptEnded {
                id: TaskId(1),
                number: 1,
                duration: Duration::from_millis(1_500),
                exit_code: Some(7),
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
                at: at(13),
            },
        );
        assert_eq!(
            journal.events().unwrap()[1..],
            [started, running, reported, ended]
        );
    }

    #[test]
    fn every_step_event_round_trips() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        let started = append(
            &journal,
            Event::StepStarted {
                id: TaskId(1),
                number: 1,
                step: "implementation".to_owned(),
                at: at(10),
            },
        );
        let ended = append(
            &journal,
            Event::StepEnded {
                id: TaskId(1),
                number: 1,
                step: "implementation".to_owned(),
                duration: Duration::from_millis(2_500),
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
                reported: Some(Outcome::Done),
                at: at(11),
            },
        );
        assert_eq!(journal.events().unwrap()[1..], [started, ended]);
    }

    #[test]
    fn a_reported_event_with_no_reason_round_trips_too() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        let reported = append(
            &journal,
            Event::AttemptReported {
                id: TaskId(1),
                number: 1,
                outcome: Outcome::Done,
                reason: None,
                step: Some("implementation".to_owned()),
                at: at(2),
            },
        );
        assert_eq!(journal.events().unwrap()[1], reported);
    }

    #[test]
    fn a_killed_attempts_ended_event_with_no_exit_code_round_trips() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        let ended = append(
            &journal,
            Event::AttemptEnded {
                id: TaskId(1),
                number: 1,
                duration: Duration::from_secs(60),
                exit_code: None,
                status: TaskStatus::FailedUnknown,
                reason: Some("killed".to_owned()),
                at: at(2),
            },
        );
        assert_eq!(journal.events().unwrap()[1], ended);
    }

    #[test]
    fn appending_a_task_added_event_records_exactly_one_row_with_the_expected_payload() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        let event = Event::TaskAdded {
            id: TaskId(1),
            draft: draft("a"),
            placement: Placement::End,
            at: at(42),
        };
        journal.append_events(&[event], 0).unwrap();
        let (count, when, kind, task_id, payload): (i64, i64, String, i64, String) = journal
            .connection
            .query_row(
                "SELECT COUNT(*), at, kind, task_id, payload FROM events",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (count, when, kind.as_str(), task_id),
            (1, 42, "task_added", 1)
        );
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({
                "title": "a",
                "body": "line one\nline \"two\"",
                "criteria": ["first", "sécond"],
                "kind": "human",
                "links": ["github:o/r#1", "https://example.com"],
            })
        );
    }

    #[test]
    fn append_events_refuses_when_the_journal_has_moved_on_and_records_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        let writer_a = SqliteJournal::open(&path).unwrap();
        let writer_b = SqliteJournal::open(&path).unwrap();
        // Both read the journal empty, then `b` appends first.
        assert_eq!(writer_a.events().unwrap().len(), 0);
        assert_eq!(writer_b.events().unwrap().len(), 0);
        add(&writer_b, 1, "from b", Placement::End);

        let event = Event::TaskAdded {
            id: TaskId(1),
            draft: draft("from a"),
            placement: Placement::End,
            at: at(1),
        };
        assert_eq!(
            writer_a.append_events(&[event], 0),
            Err(AppendConflict::Conflict)
        );
        // Nothing from `a`'s stale attempt was recorded; only `b`'s event is there.
        assert_eq!(writer_a.events().unwrap().len(), 1);
    }

    #[test]
    fn append_events_is_atomic_a_batch_that_fails_part_way_is_rolled_back() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON events
                 WHEN (SELECT COUNT(*) FROM events) >= 1
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
        let batch: Vec<_> = ["x", "y"]
            .into_iter()
            .enumerate()
            .map(|(index, title)| Event::TaskAdded {
                id: TaskId(index as u64 + 1),
                draft: draft(title),
                placement: Placement::End,
                at: at(2),
            })
            .collect();
        let error = journal.append_events(&batch, 0).unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
        assert_eq!(journal.events().unwrap(), vec![]);
        let cached: i64 = journal
            .connection
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cached, 0, "the tasks cache is rolled back too");
    }

    #[test]
    fn a_failed_append_records_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .connection
            .execute_batch("DROP TABLE events")
            .unwrap();
        let event = Event::TaskAdded {
            id: TaskId(1),
            draft: draft("a"),
            placement: Placement::End,
            at: at(1),
        };
        let error = journal.append_events(&[event], 0).unwrap_err();
        assert!(error.to_string().contains("cannot append"), "{error}");
        let cached: i64 = journal
            .connection
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cached, 0);
    }

    #[test]
    fn appending_mirrors_every_kind_of_event_into_the_tasks_cache() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        assert_eq!(cached(&journal, 1), (TaskStatus::Pending, 0));

        append(
            &journal,
            Event::AttemptStarted {
                id: TaskId(1),
                number: 1,
                at: at(2),
            },
        );
        assert_eq!(cached(&journal, 1), (TaskStatus::Running, 1));

        // Running and a report change no status.
        append(
            &journal,
            Event::AttemptRunning {
                id: TaskId(1),
                number: 1,
                provider: "echo".to_owned(),
                at: at(3),
            },
        );
        append(
            &journal,
            Event::AttemptReported {
                id: TaskId(1),
                number: 1,
                outcome: Outcome::Done,
                reason: None,
                step: None,
                at: at(4),
            },
        );
        assert_eq!(cached(&journal, 1), (TaskStatus::Running, 1));

        append(
            &journal,
            Event::AttemptEnded {
                id: TaskId(1),
                number: 1,
                duration: Duration::from_secs(1),
                exit_code: Some(0),
                status: TaskStatus::Done,
                reason: None,
                at: at(5),
            },
        );
        assert_eq!(cached(&journal, 1), (TaskStatus::Done, 1));

        add(&journal, 2, "b", Placement::End);
        cancel(&journal, 2, at(6));
        assert_eq!(cached(&journal, 2), (TaskStatus::Cancelled, 0));
    }

    #[test]
    fn an_events_table_written_by_the_previous_version_is_still_read_correctly() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        {
            // The schema and the rows exactly as the previous version — which kept
            // `begin_attempt`, `end_attempt` and the rest on the `Journal` trait itself
            // instead of as events this adapter merely stores — wrote them: one `tasks` row
            // kept by hand, and the same `events` rows `task_added` and `task_cancelled`
            // always had.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE events (
                         seq INTEGER PRIMARY KEY AUTOINCREMENT,
                         at INTEGER NOT NULL,
                         kind TEXT NOT NULL,
                         task_id INTEGER NOT NULL,
                         payload TEXT NOT NULL
                     );
                     CREATE TABLE tasks (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         order_key INTEGER NOT NULL,
                         title TEXT NOT NULL,
                         body TEXT NOT NULL,
                         criteria TEXT NOT NULL,
                         kind TEXT NOT NULL,
                         links TEXT NOT NULL,
                         status TEXT NOT NULL,
                         created_at INTEGER NOT NULL,
                         attempt_number INTEGER NOT NULL DEFAULT 0
                     )",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO tasks
                         (id, order_key, title, body, criteria, kind, links, status, created_at)
                     VALUES (1, 1, 'old task', '', '[\"c\"]', 'agent', '[]', 'cancelled', 100)",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO events (at, kind, task_id, payload) VALUES (100, 'task_added', 1, ?1)",
                    [r#"{"title":"old task","body":"","criteria":["c"],"kind":"agent","links":[]}"#],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO events (at, kind, task_id, payload) VALUES (200, 'task_cancelled', 1, '{}')",
                    [],
                )
                .unwrap();
        }

        let journal = SqliteJournal::open(&path).unwrap();
        assert_eq!(
            journal.events().unwrap(),
            vec![
                Event::TaskAdded {
                    id: TaskId(1),
                    draft: TaskDraft {
                        title: "old task".to_owned(),
                        body: String::new(),
                        criteria: vec!["c".to_owned()],
                        kind: TaskKind::Agent,
                        links: vec![],
                    },
                    placement: Placement::End,
                    at: at(100),
                },
                Event::TaskCancelled {
                    id: TaskId(1),
                    at: at(200),
                },
            ]
        );
        // New events append correctly after it, with the next id continuing on from the
        // highest one the old journal ever used, and mirror into the pre-existing row.
        add(&journal, 2, "new", Placement::End);
        assert_eq!(journal.events().unwrap().len(), 3);
        append(
            &journal,
            Event::AttemptStarted {
                id: TaskId(2),
                number: 1,
                at: at(300),
            },
        );
        assert_eq!(cached(&journal, 2), (TaskStatus::Running, 1));
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error_naming_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        std::fs::write(
            &path,
            "this is not sqlite, and it is long enough to be checked",
        )
        .unwrap();
        let error = SqliteJournal::open(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_an_error() {
        let dir = TempDir::new().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let error = SqliteJournal::open(&blocker.join("journal.db")).unwrap_err();
        assert!(error.to_string().contains("state directory"), "{error}");
    }
}

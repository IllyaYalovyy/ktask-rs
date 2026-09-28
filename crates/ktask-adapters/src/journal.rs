//! A project's journal in SQLite.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ktask_core::{
    AppendConflict, Attempt, AttemptEnd, AttemptRun, BeginAttemptError, Event, Journal,
    JournalError, Outcome, Placement, RecordReportError, TaskDraft, TaskId, TaskKind, TaskStatus,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;

/// How long a writer waits for another process's transaction to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A project's journal, kept in a SQLite database file.
///
/// `events` is the journal proper: one row per event, appended and never changed — the only
/// thing this adapter decides is whether the count it was given still matches the table's;
/// everything about what an event means (positions, numbers, which placements are valid) is
/// `ktask_core::queue_state`'s. `tasks` is a mechanical mirror of the queue events in
/// `events`, kept only so the attempt-tracking methods below have an id-keyed row to read and
/// write, updated in the same transaction as the event that changes it.
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

/// The kinds of event `events` and `append_events` know: the queue's own events. Every other
/// kind recorded in the same table — the attempt-tracking ones — is the unrelated business of
/// the methods below that still read and write `tasks` directly.
const TASK_ADDED: &str = "task_added";
const TASK_CANCELLED: &str = "task_cancelled";

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

/// The event a `task_added` or `task_cancelled` row decodes to.
fn decode_event(kind: &str, task_id: i64, at: i64, payload: &str) -> Result<Event, JournalError> {
    let doing = "cannot read the journal's events";
    let corrupt = |what: &str, cause: String| {
        failed(
            doing,
            format!("event {kind} for task {task_id} has a bad {what}: {cause}"),
        )
    };
    let id = TaskId(u64::try_from(task_id).map_err(|e| corrupt("task id", e.to_string()))?);
    let at = from_seconds(at);
    if kind == TASK_CANCELLED {
        return Ok(Event::TaskCancelled { id, at });
    }
    let payload: Value =
        serde_json::from_str(payload).map_err(|e| corrupt("payload", e.to_string()))?;
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
    let draft = TaskDraft {
        title: field("title")?,
        body: field("body")?,
        criteria: strings("criteria")?,
        kind: field("kind")?
            .parse::<TaskKind>()
            .map_err(|e| corrupt("kind", e))?,
        links: strings("links")?,
    };
    let placement = if let Some(before) = payload.get("before").and_then(Value::as_u64) {
        Placement::Before(TaskId(before))
    } else if let Some(after) = payload.get("after").and_then(Value::as_u64) {
        Placement::After(TaskId(after))
    } else {
        Placement::End
    };
    Ok(Event::TaskAdded {
        id,
        draft,
        placement,
        at,
    })
}

/// Mirrors `event` into the `tasks` cache, inside `transaction`: mechanical bookkeeping for
/// the attempt-tracking methods below, which still read and write `tasks` directly — no rule
/// about the queue is decided here.
fn mirror(transaction: &Transaction<'_>, event: &Event) -> Result<(), rusqlite::Error> {
    match event {
        Event::TaskAdded { id, draft, at, .. } => {
            let criteria = serde_json::json!(draft.criteria).to_string();
            let links = serde_json::json!(draft.links).to_string();
            transaction.execute(
                "INSERT INTO tasks
                     (id, order_key, title, body, criteria, kind, links, status, created_at)
                 VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    i64::try_from(id.0).unwrap_or(i64::MAX),
                    &draft.title,
                    &draft.body,
                    &criteria,
                    draft.kind.as_str(),
                    &links,
                    TaskStatus::Pending.as_str(),
                    to_seconds(*at),
                ),
            )?;
        }
        Event::TaskCancelled { id, .. } => {
            transaction.execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                (
                    i64::try_from(id.0).unwrap_or(i64::MAX),
                    TaskStatus::Cancelled.as_str(),
                ),
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

impl SqliteJournal {
    /// The most recent event of `kind` for task `task_id`'s attempt `number`: its `at` and
    /// payload, or `None` when there is no such event.
    fn attempt_event(
        &self,
        task_id: i64,
        number: u32,
        kind: &str,
    ) -> Result<Option<(i64, Value)>, JournalError> {
        let doing = "cannot read the task's attempt";
        let mut statement = self
            .connection
            .prepare(
                "SELECT at, payload FROM events
                 WHERE kind = ?1 AND task_id = ?2 ORDER BY seq DESC",
            )
            .map_err(|e| failed(doing, e))?;
        let rows = statement
            .query_map((kind, task_id), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| failed(doing, e))?;
        for row in rows {
            let (at, payload) = row.map_err(|e| failed(doing, e))?;
            let payload: Value = serde_json::from_str(&payload).map_err(|e| failed(doing, e))?;
            if payload.get("number").and_then(Value::as_u64) == Some(u64::from(number)) {
                return Ok(Some((at, payload)));
            }
        }
        Ok(None)
    }
}

impl Journal for SqliteJournal {
    fn events(&self) -> Result<Vec<Event>, JournalError> {
        let doing = "cannot read the journal's events";
        let mut statement = self
            .connection
            .prepare(
                "SELECT at, kind, task_id, payload FROM events
                 WHERE kind IN (?1, ?2) ORDER BY seq",
            )
            .map_err(|e| failed(doing, e))?;
        let rows = statement
            .query_map([TASK_ADDED, TASK_CANCELLED], |row| {
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
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN (?1, ?2)",
                [TASK_ADDED, TASK_CANCELLED],
                |row| row.get(0),
            )
            .map_err(|e| failed(doing, e))?;
        if usize::try_from(current).unwrap_or(usize::MAX) != read {
            return Err(AppendConflict::Conflict);
        }
        for event in events {
            let (kind, task_id, at, payload) = match event {
                Event::TaskAdded {
                    id,
                    draft,
                    placement,
                    at,
                } => (
                    TASK_ADDED,
                    i64::try_from(id.0).unwrap_or(i64::MAX),
                    to_seconds(*at),
                    task_added_payload(draft, *placement),
                ),
                Event::TaskCancelled { id, at } => (
                    TASK_CANCELLED,
                    i64::try_from(id.0).unwrap_or(i64::MAX),
                    to_seconds(*at),
                    "{}".to_owned(),
                ),
            };
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

    fn begin_attempt(&self, id: TaskId, at: SystemTime) -> Result<u32, BeginAttemptError> {
        let doing = "cannot start the attempt";
        let number_id = i64::try_from(id.0).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let found = transaction
            .query_row(
                "SELECT status, attempt_number FROM tasks WHERE id = ?1",
                [number_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| failed(doing, e))?;
        let Some((status, current)) = found else {
            return Err(BeginAttemptError::UnknownTask(id));
        };
        if status != TaskStatus::Pending.as_str() {
            return Err(BeginAttemptError::NotPending(id));
        }
        let number = current + 1;
        transaction
            .execute(
                "UPDATE tasks SET status = ?2, attempt_number = ?3 WHERE id = ?1",
                (number_id, TaskStatus::Running.as_str(), number),
            )
            .map_err(|e| failed(doing, e))?;
        transaction
            .execute(
                "INSERT INTO events (at, kind, task_id, payload)
                 VALUES (?1, 'attempt_started', ?2, ?3)",
                (
                    to_seconds(at),
                    number_id,
                    serde_json::json!({ "number": number }).to_string(),
                ),
            )
            .map_err(|e| failed(doing, e))?;
        transaction.commit().map_err(|e| failed(doing, e))?;
        u32::try_from(number).map_err(|e| failed(doing, e).into())
    }

    fn record_report(
        &self,
        id: TaskId,
        number: u32,
        outcome: Outcome,
        reason: Option<&str>,
        at: SystemTime,
    ) -> Result<(), RecordReportError> {
        let doing = "cannot record the report";
        let number_id = i64::try_from(id.0).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let found = transaction
            .query_row(
                "SELECT status, attempt_number FROM tasks WHERE id = ?1",
                [number_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| failed(doing, e))?;
        match found {
            None => return Err(RecordReportError::UnknownAttempt { task: id, number }),
            Some((_, current)) if current != i64::from(number) => {
                return Err(RecordReportError::UnknownAttempt { task: id, number });
            }
            Some((status, _)) if status != TaskStatus::Running.as_str() => {
                return Err(RecordReportError::AttemptEnded { task: id, number });
            }
            Some(_) => {}
        }
        transaction
            .execute(
                "INSERT INTO events (at, kind, task_id, payload)
                 VALUES (?1, 'attempt_reported', ?2, ?3)",
                (
                    to_seconds(at),
                    number_id,
                    serde_json::json!({
                        "number": number,
                        "outcome": outcome.as_str(),
                        "reason": reason,
                    })
                    .to_string(),
                ),
            )
            .map_err(|e| failed(doing, e))?;
        transaction.commit().map_err(|e| failed(doing, e))?;
        Ok(())
    }

    fn attempt_running(
        &self,
        id: TaskId,
        number: u32,
        provider: &str,
        at: SystemTime,
    ) -> Result<(), JournalError> {
        let doing = "cannot record that the attempt is running";
        let number_id = i64::try_from(id.0).unwrap_or(i64::MAX);
        self.connection
            .execute(
                "INSERT INTO events (at, kind, task_id, payload)
                 VALUES (?1, 'attempt_running', ?2, ?3)",
                (
                    to_seconds(at),
                    number_id,
                    serde_json::json!({ "number": number, "provider": provider }).to_string(),
                ),
            )
            .map_err(|e| failed(doing, e))?;
        Ok(())
    }

    fn last_report(
        &self,
        id: TaskId,
        number: u32,
    ) -> Result<Option<(Outcome, Option<String>)>, JournalError> {
        let doing = "cannot read the attempt's report";
        let number_id = i64::try_from(id.0).unwrap_or(i64::MAX);
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload FROM events
                 WHERE kind = 'attempt_reported' AND task_id = ?1
                 ORDER BY seq DESC",
            )
            .map_err(|e| failed(doing, e))?;
        let payloads = statement
            .query_map([number_id], |row| row.get::<_, String>(0))
            .map_err(|e| failed(doing, e))?;
        for payload in payloads {
            let payload = payload.map_err(|e| failed(doing, e))?;
            let payload: Value = serde_json::from_str(&payload).map_err(|e| failed(doing, e))?;
            if payload.get("number").and_then(Value::as_u64) != Some(u64::from(number)) {
                continue;
            }
            let outcome = payload
                .get("outcome")
                .and_then(Value::as_str)
                .ok_or_else(|| failed(doing, "a report event has no outcome"))?
                .parse::<Outcome>()
                .map_err(|e| failed(doing, e))?;
            let reason = payload
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            return Ok(Some((outcome, reason)));
        }
        Ok(None)
    }

    fn end_attempt(
        &self,
        id: TaskId,
        number: u32,
        run: AttemptRun<'_>,
        at: SystemTime,
    ) -> Result<(), RecordReportError> {
        let doing = "cannot end the attempt";
        let number_id = i64::try_from(id.0).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let found = transaction
            .query_row(
                "SELECT status, attempt_number FROM tasks WHERE id = ?1",
                [number_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| failed(doing, e))?;
        match found {
            None => return Err(RecordReportError::UnknownAttempt { task: id, number }),
            Some((_, current)) if current != i64::from(number) => {
                return Err(RecordReportError::UnknownAttempt { task: id, number });
            }
            Some((status, _)) if status != TaskStatus::Running.as_str() => {
                return Err(RecordReportError::AttemptEnded { task: id, number });
            }
            Some(_) => {}
        }
        transaction
            .execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                (number_id, run.status.as_str()),
            )
            .map_err(|e| failed(doing, e))?;
        transaction
            .execute(
                "INSERT INTO events (at, kind, task_id, payload)
                 VALUES (?1, 'attempt_ended', ?2, ?3)",
                (
                    to_seconds(at),
                    number_id,
                    serde_json::json!({
                        "number": number,
                        "duration_ms":
                            i64::try_from(run.duration.as_millis()).unwrap_or(i64::MAX),
                        "exit_code": run.exit_code,
                        "status": run.status.as_str(),
                        "reason": run.reason,
                    })
                    .to_string(),
                ),
            )
            .map_err(|e| failed(doing, e))?;
        transaction.commit().map_err(|e| failed(doing, e))?;
        Ok(())
    }

    fn running(&self) -> Result<Option<(TaskId, u32)>, JournalError> {
        let doing = "cannot read the running task";
        self.connection
            .query_row(
                "SELECT id, attempt_number FROM tasks WHERE status = ?1 LIMIT 1",
                [TaskStatus::Running.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| failed(doing, e))?
            .map(|(id, number)| {
                Ok((
                    TaskId(u64::try_from(id).map_err(|e| failed(doing, e))?),
                    u32::try_from(number).map_err(|e| failed(doing, e))?,
                ))
            })
            .transpose()
    }

    fn last_attempt(&self, id: TaskId) -> Result<Option<Attempt>, JournalError> {
        let doing = "cannot read the task's attempt";
        let number_id = i64::try_from(id.0).unwrap_or(i64::MAX);
        let attempt_number = self
            .connection
            .query_row(
                "SELECT attempt_number FROM tasks WHERE id = ?1",
                [number_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| failed(doing, e))?;
        let Some(attempt_number) = attempt_number.filter(|number| *number > 0) else {
            return Ok(None);
        };
        let number = u32::try_from(attempt_number).map_err(|e| failed(doing, e))?;

        let Some((started, _)) = self.attempt_event(number_id, number, "attempt_started")? else {
            return Err(failed(
                doing,
                format!("task {id} has attempt {number} but no attempt_started event"),
            ));
        };
        let provider = self
            .attempt_event(number_id, number, "attempt_running")?
            .and_then(|(_, payload)| {
                payload
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let ended = self
            .attempt_event(number_id, number, "attempt_ended")?
            .map(|(_, payload)| -> Result<AttemptEnd, JournalError> {
                let duration_ms = payload
                    .get("duration_ms")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| failed(doing, "an attempt_ended event has no duration"))?;
                let status = payload
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or_else(|| failed(doing, "an attempt_ended event has no status"))?
                    .parse::<TaskStatus>()
                    .map_err(|e| failed(doing, e))?;
                let reason = payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Ok(AttemptEnd {
                    duration: Duration::from_millis(
                        u64::try_from(duration_ms).map_err(|e| failed(doing, e))?,
                    ),
                    status,
                    reason,
                })
            })
            .transpose()?;

        Ok(Some(Attempt {
            number,
            started_at: from_seconds(started),
            provider,
            ended,
        }))
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

    /// Appends a `task_added` event for a task numbered `id`, titled `title`, at `placement`
    /// — as `ktask_core::task::add_tasks` would after deciding it — and returns the event.
    fn add(journal: &SqliteJournal, id: u64, title: &str, placement: Placement) -> Event {
        let read = journal.events().unwrap().len();
        let event = Event::TaskAdded {
            id: TaskId(id),
            draft: draft(title),
            placement,
            at: at(1),
        };
        journal
            .append_events(std::slice::from_ref(&event), read)
            .unwrap();
        event
    }

    /// Appends a `task_cancelled` event for task `id` at `moment`.
    fn cancel(journal: &SqliteJournal, id: u64, moment: SystemTime) {
        let read = journal.events().unwrap().len();
        journal
            .append_events(
                &[Event::TaskCancelled {
                    id: TaskId(id),
                    at: moment,
                }],
                read,
            )
            .unwrap();
    }

    /// The `tasks` cache row's status for task `id` — what the attempt-tracking methods,
    /// unrelated to and untouched by this task, see.
    fn cached_status(journal: &SqliteJournal, id: u64) -> TaskStatus {
        journal
            .connection
            .query_row(
                "SELECT status FROM tasks WHERE id = ?1",
                [i64::try_from(id).unwrap_or(i64::MAX)],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
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
                Event::TaskCancelled { .. } => unreachable!("only additions were made"),
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
                Event::TaskCancelled { .. } => unreachable!("only additions were made"),
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
    fn appending_mirrors_into_the_tasks_cache_so_attempt_tracking_keeps_working() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        assert_eq!(cached_status(&journal, 1), TaskStatus::Pending);
        // The attempt-tracking methods, unchanged, read the mirror: a pending task may
        // start an attempt.
        journal.begin_attempt(TaskId(1), at(2)).unwrap();
        assert_eq!(cached_status(&journal, 1), TaskStatus::Running);

        add(&journal, 2, "b", Placement::End);
        cancel(&journal, 2, at(3));
        assert_eq!(cached_status(&journal, 2), TaskStatus::Cancelled);
        assert_eq!(
            journal.begin_attempt(TaskId(2), at(4)),
            Err(BeginAttemptError::NotPending(TaskId(2)))
        );
    }

    #[test]
    fn an_events_table_written_by_the_previous_version_is_still_read_correctly() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        {
            // The schema and the rows exactly as the previous version, which had no
            // `append_events`, wrote them: one `tasks` row kept by hand, and the same
            // `events` rows `task_added` and `task_cancelled` always had.
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
        // The attempt-tracking methods, untouched by this task, still work against the
        // pre-existing `tasks` row.
        assert_eq!(
            journal.begin_attempt(TaskId(1), at(300)),
            Err(BeginAttemptError::NotPending(TaskId(1)))
        );
        // New events append correctly after it, with the next id continuing on from the
        // highest one the old journal ever used.
        add(&journal, 2, "new", Placement::End);
        assert_eq!(journal.events().unwrap().len(), 3);
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

    #[test]
    fn starting_an_attempt_numbers_it_from_one_marks_the_task_running_and_records_one_event() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        let number = journal.begin_attempt(TaskId(1), at(50)).unwrap();
        assert_eq!(number, 1);
        assert_eq!(cached_status(&journal, 1), TaskStatus::Running);
        let (kind, task_id, when, payload): (String, i64, i64, String) = journal
            .connection
            .query_row(
                "SELECT kind, task_id, at, payload FROM events WHERE seq = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), task_id, when), ("attempt_started", 1, 50));
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload, serde_json::json!({ "number": 1 }));
    }

    #[test]
    fn starting_an_attempt_at_an_unknown_or_a_non_pending_task_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        assert_eq!(
            journal.begin_attempt(TaskId(9), at(1)),
            Err(BeginAttemptError::UnknownTask(TaskId(9)))
        );
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        assert_eq!(
            journal.begin_attempt(TaskId(1), at(2)),
            Err(BeginAttemptError::NotPending(TaskId(1)))
        );
        let events: i64 = journal
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 2);
    }

    #[test]
    fn a_valid_report_is_recorded_and_a_second_one_is_recorded_too() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();

        journal
            .record_report(TaskId(1), 1, Outcome::Failed, Some("first try"), at(10))
            .unwrap();
        journal
            .record_report(TaskId(1), 1, Outcome::Done, None, at(20))
            .unwrap();

        let events: i64 = journal
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'attempt_reported'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 2);
        let (at_last, payload): (i64, String) = journal
            .connection
            .query_row(
                "SELECT at, payload FROM events WHERE kind = 'attempt_reported'
                 ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(at_last, 20);
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "number": 1, "outcome": "done", "reason": null })
        );
        // The task stays running: reporting alone does not end the attempt.
        assert_eq!(cached_status(&journal, 1), TaskStatus::Running);
    }

    #[test]
    fn a_report_for_an_unknown_task_or_the_wrong_attempt_number_is_refused_and_records_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        assert_eq!(
            journal.record_report(TaskId(9), 1, Outcome::Done, None, at(2)),
            Err(RecordReportError::UnknownAttempt {
                task: TaskId(9),
                number: 1
            })
        );
        assert_eq!(
            journal.record_report(TaskId(1), 2, Outcome::Done, None, at(2)),
            Err(RecordReportError::UnknownAttempt {
                task: TaskId(1),
                number: 2
            })
        );
        let events: i64 = journal
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'attempt_reported'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn a_report_for_an_ended_attempt_is_refused_and_records_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        cancel(&journal, 1, at(2));
        assert_eq!(
            journal.record_report(TaskId(1), 1, Outcome::Done, None, at(3)),
            Err(RecordReportError::AttemptEnded {
                task: TaskId(1),
                number: 1
            })
        );
        let events: i64 = journal
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'attempt_reported'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn attempt_running_records_one_event_with_the_provider_and_changes_no_status() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();

        journal
            .attempt_running(TaskId(1), 1, "echo", at(5))
            .unwrap();

        assert_eq!(cached_status(&journal, 1), TaskStatus::Running);
        let (kind, task_id, when, payload): (String, i64, i64, String) = journal
            .connection
            .query_row(
                "SELECT kind, task_id, at, payload FROM events WHERE kind = 'attempt_running'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), task_id, when), ("attempt_running", 1, 5));
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "number": 1, "provider": "echo" })
        );
    }

    #[test]
    fn last_report_is_none_until_the_agent_reports_and_then_the_most_recent_one() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();

        assert_eq!(journal.last_report(TaskId(1), 1).unwrap(), None);

        journal
            .record_report(TaskId(1), 1, Outcome::Failed, Some("first try"), at(10))
            .unwrap();
        assert_eq!(
            journal.last_report(TaskId(1), 1).unwrap(),
            Some((Outcome::Failed, Some("first try".to_owned())))
        );

        journal
            .record_report(TaskId(1), 1, Outcome::Done, None, at(20))
            .unwrap();
        assert_eq!(
            journal.last_report(TaskId(1), 1).unwrap(),
            Some((Outcome::Done, None))
        );
    }

    #[test]
    fn last_report_is_scoped_to_the_attempt_number_it_is_asked_for() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        journal
            .record_report(TaskId(1), 1, Outcome::Done, None, at(2))
            .unwrap();

        assert_eq!(journal.last_report(TaskId(1), 2).unwrap(), None);
        assert_eq!(journal.last_report(TaskId(9), 1).unwrap(), None);
    }

    #[test]
    fn end_attempt_sets_the_status_and_records_one_event_with_what_happened() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();

        journal
            .end_attempt(
                TaskId(1),
                1,
                AttemptRun {
                    duration: Duration::from_millis(1_500),
                    exit_code: Some(7),
                    status: TaskStatus::Failed,
                    reason: Some("it broke"),
                },
                at(30),
            )
            .unwrap();

        assert_eq!(cached_status(&journal, 1), TaskStatus::Failed);
        let (kind, task_id, when, payload): (String, i64, i64, String) = journal
            .connection
            .query_row(
                "SELECT kind, task_id, at, payload FROM events WHERE kind = 'attempt_ended'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), task_id, when), ("attempt_ended", 1, 30));
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({
                "number": 1,
                "duration_ms": 1_500,
                "exit_code": 7,
                "status": "failed",
                "reason": "it broke",
            })
        );
    }

    #[test]
    fn end_attempt_for_an_unknown_or_wrong_attempt_is_refused_and_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        let run = AttemptRun {
            duration: Duration::from_secs(1),
            exit_code: Some(0),
            status: TaskStatus::Done,
            reason: None,
        };
        assert_eq!(
            journal.end_attempt(TaskId(9), 1, run, at(2)),
            Err(RecordReportError::UnknownAttempt {
                task: TaskId(9),
                number: 1
            })
        );
        assert_eq!(
            journal.end_attempt(TaskId(1), 2, run, at(2)),
            Err(RecordReportError::UnknownAttempt {
                task: TaskId(1),
                number: 2
            })
        );
        assert_eq!(cached_status(&journal, 1), TaskStatus::Running);
    }

    #[test]
    fn end_attempt_for_an_already_ended_attempt_is_refused_and_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        cancel(&journal, 1, at(2));
        let run = AttemptRun {
            duration: Duration::from_secs(1),
            exit_code: Some(0),
            status: TaskStatus::Done,
            reason: None,
        };
        assert_eq!(
            journal.end_attempt(TaskId(1), 1, run, at(3)),
            Err(RecordReportError::AttemptEnded {
                task: TaskId(1),
                number: 1
            })
        );
        assert_eq!(cached_status(&journal, 1), TaskStatus::Cancelled);
    }

    #[test]
    fn running_is_none_until_an_attempt_starts_and_names_its_task_and_attempt_number() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        assert_eq!(journal.running().unwrap(), None);

        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        assert_eq!(journal.running().unwrap(), Some((TaskId(1), 1)));
    }

    #[test]
    fn running_is_none_again_once_the_attempt_ends() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        journal
            .end_attempt(
                TaskId(1),
                1,
                AttemptRun {
                    duration: Duration::from_secs(1),
                    exit_code: Some(0),
                    status: TaskStatus::Done,
                    reason: None,
                },
                at(2),
            )
            .unwrap();
        assert_eq!(journal.running().unwrap(), None);
    }

    #[test]
    fn last_attempt_is_none_for_a_task_never_attempted() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        assert_eq!(journal.last_attempt(TaskId(1)).unwrap(), None);
        assert_eq!(journal.last_attempt(TaskId(9)).unwrap(), None);
    }

    #[test]
    fn last_attempt_while_running_carries_its_number_start_and_provider_but_no_ending() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(50)).unwrap();
        journal
            .attempt_running(TaskId(1), 1, "echo", at(55))
            .unwrap();

        let attempt = journal.last_attempt(TaskId(1)).unwrap().unwrap();
        assert_eq!(attempt.number, 1);
        assert_eq!(attempt.started_at, at(50));
        assert_eq!(attempt.provider, Some("echo".to_owned()));
        assert_eq!(attempt.ended, None);
    }

    #[test]
    fn last_attempt_once_ended_carries_its_duration_status_and_reason() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        add(&journal, 1, "a", Placement::End);
        journal.begin_attempt(TaskId(1), at(50)).unwrap();
        journal
            .attempt_running(TaskId(1), 1, "echo", at(55))
            .unwrap();
        journal
            .end_attempt(
                TaskId(1),
                1,
                AttemptRun {
                    duration: Duration::from_millis(1_500),
                    exit_code: Some(1),
                    status: TaskStatus::Failed,
                    reason: Some("it broke"),
                },
                at(70),
            )
            .unwrap();

        let attempt = journal.last_attempt(TaskId(1)).unwrap().unwrap();
        assert_eq!(attempt.number, 1);
        assert_eq!(attempt.started_at, at(50));
        assert_eq!(attempt.provider, Some("echo".to_owned()));
        assert_eq!(
            attempt.ended,
            Some(AttemptEnd {
                duration: Duration::from_millis(1_500),
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
            })
        );
    }
}

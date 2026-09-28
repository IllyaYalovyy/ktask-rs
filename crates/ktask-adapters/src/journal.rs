//! A project's journal in SQLite.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ktask_core::{
    AppendError, Attempt, AttemptEnd, AttemptRun, BeginAttemptError, CancelError, Journal,
    JournalError, Outcome, Placement, RecordReportError, Task, TaskDraft, TaskId, TaskKind,
    TaskStatus,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;

/// How long a writer waits for another process's transaction to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A project's journal, kept in a SQLite database file.
///
/// `events` is the journal proper: one row per event, appended and never changed. `tasks` is
/// the projection of it, changed only in the transaction that appends the event. Task
/// numbers come from `AUTOINCREMENT`, which never hands out a number twice; the queue order
/// is the separate `order_key`, which inserting a task shifts and a number never follows.
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

/// The order key a task placed at `placement` takes, after making room for it by moving
/// every task from that key on one place later.
///
/// # Errors
///
/// Fails when `placement` names a task that does not exist or was cancelled, or when the
/// database fails.
fn make_room(transaction: &Transaction<'_>, placement: Placement) -> Result<i64, AppendError> {
    let doing = "cannot add the task to the journal";
    let (anchor, offset) = match placement {
        Placement::End => {
            let last = transaction
                .query_row("SELECT COALESCE(MAX(order_key), 0) FROM tasks", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| failed(doing, e))?;
            return Ok(last + 1);
        }
        Placement::Before(anchor) => (anchor, 0),
        Placement::After(anchor) => (anchor, 1),
    };
    let found = transaction
        .query_row(
            "SELECT order_key, status FROM tasks WHERE id = ?1",
            [i64::try_from(anchor.0).unwrap_or(i64::MAX)],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|e| failed(doing, e))?;
    let Some((order_key, status)) = found else {
        return Err(AppendError::UnknownTask(anchor));
    };
    if status == TaskStatus::Cancelled.as_str() {
        return Err(AppendError::CancelledTask(anchor));
    }
    let key = order_key + offset;
    transaction
        .execute(
            "UPDATE tasks SET order_key = order_key + 1 WHERE order_key >= ?1",
            [key],
        )
        .map_err(|e| failed(doing, e))?;
    Ok(key)
}

/// Inserts `draft` with order key `order_key` and records the event, inside `transaction`.
/// Returns the new task's number.
fn insert(
    transaction: &Transaction<'_>,
    draft: &TaskDraft,
    placement: Placement,
    order_key: i64,
    created_at: i64,
) -> Result<i64, rusqlite::Error> {
    let criteria = serde_json::json!(draft.criteria).to_string();
    let links = serde_json::json!(draft.links).to_string();
    transaction.execute(
        "INSERT INTO tasks
             (order_key, title, body, criteria, kind, links, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            order_key,
            &draft.title,
            &draft.body,
            &criteria,
            draft.kind.as_str(),
            &links,
            TaskStatus::Pending.as_str(),
            created_at,
        ),
    )?;
    let id = transaction.last_insert_rowid();
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
    transaction.execute(
        "INSERT INTO events (at, kind, task_id, payload) VALUES (?1, 'task_added', ?2, ?3)",
        (created_at, id, Value::Object(payload).to_string()),
    )?;
    Ok(id)
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
    fn append_tasks(
        &self,
        drafts: &[TaskDraft],
        placement: Placement,
        at: SystemTime,
    ) -> Result<Vec<Task>, AppendError> {
        let doing = "cannot add the task to the journal";
        // Immediate: take the write lock first, so that reading the order keys and
        // inserting among them cannot interleave with another process adding a task.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let mut added = Vec::new();
        let mut placement = placement;
        for draft in drafts {
            let order_key = make_room(&transaction, placement)?;
            let id = insert(&transaction, draft, placement, order_key, to_seconds(at))
                .map_err(|e| failed(doing, e))?;
            let position: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE order_key <= ?1",
                    [order_key],
                    |row| row.get(0),
                )
                .map_err(|e| failed(doing, e))?;
            let id = TaskId(u64::try_from(id).map_err(|e| failed(doing, e))?);
            added.push(Task {
                id,
                position: usize::try_from(position).map_err(|e| failed(doing, e))?,
                title: draft.title.clone(),
                body: draft.body.clone(),
                criteria: draft.criteria.clone(),
                kind: draft.kind,
                links: draft.links.clone(),
                status: TaskStatus::Pending,
                created_at: from_seconds(to_seconds(at)),
            });
            placement = placement.then_after(id);
        }
        transaction.commit().map_err(|e| failed(doing, e))?;
        Ok(added)
    }

    fn cancel_task(&self, id: TaskId, at: SystemTime) -> Result<(), CancelError> {
        let doing = "cannot remove the task from the journal";
        let number = i64::try_from(id.0).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let status = transaction
            .query_row("SELECT status FROM tasks WHERE id = ?1", [number], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .map_err(|e| failed(doing, e))?;
        match status {
            None => return Err(CancelError::UnknownTask(id)),
            Some(status) if status == TaskStatus::Cancelled.as_str() => {
                return Err(CancelError::AlreadyCancelled(id));
            }
            Some(_) => {}
        }
        transaction
            .execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                (number, TaskStatus::Cancelled.as_str()),
            )
            .map_err(|e| failed(doing, e))?;
        transaction
            .execute(
                "INSERT INTO events (at, kind, task_id, payload)
                 VALUES (?1, 'task_cancelled', ?2, '{}')",
                (to_seconds(at), number),
            )
            .map_err(|e| failed(doing, e))?;
        transaction.commit().map_err(|e| failed(doing, e))?;
        Ok(())
    }

    fn tasks(&self) -> Result<Vec<Task>, JournalError> {
        let doing = "cannot read the tasks from the journal";
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, title, body, criteria, kind, links, status, created_at
                 FROM tasks ORDER BY order_key",
            )
            .map_err(|e| failed(doing, e))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .map_err(|e| failed(doing, e))?;
        let mut tasks = Vec::new();
        for (index, row) in rows.enumerate() {
            let (id, title, body, criteria, kind, links, status, created_at) =
                row.map_err(|e| failed(doing, e))?;
            let corrupt = |what: &str, cause: String| {
                failed(doing, format!("task {id} has a bad {what}: {cause}"))
            };
            tasks.push(Task {
                id: TaskId(u64::try_from(id).map_err(|e| corrupt("id", e.to_string()))?),
                position: index + 1,
                title,
                body,
                criteria: serde_json::from_str(&criteria)
                    .map_err(|e| corrupt("criteria", e.to_string()))?,
                kind: kind.parse::<TaskKind>().map_err(|e| corrupt("kind", e))?,
                links: serde_json::from_str(&links).map_err(|e| corrupt("links", e.to_string()))?,
                status: status
                    .parse::<TaskStatus>()
                    .map_err(|e| corrupt("status", e))?,
                created_at: from_seconds(created_at),
            });
        }
        Ok(tasks)
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

    #[test]
    fn a_new_journal_is_created_with_its_directory_and_holds_no_tasks() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("journal.db");
        let journal = SqliteJournal::open(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(journal.tasks(), Ok(vec![]));
    }

    #[test]
    fn an_added_task_is_returned_and_read_back_as_written_after_reopening() {
        let dir = TempDir::new().unwrap();
        let added = open(&dir)
            .append_task(&draft("t"), Placement::End, at(700))
            .unwrap();
        assert_eq!(
            added,
            Task {
                id: TaskId(1),
                position: 1,
                title: "t".to_owned(),
                body: "line one\nline \"two\"".to_owned(),
                criteria: vec!["first".to_owned(), "sécond".to_owned()],
                kind: TaskKind::Human,
                links: vec!["github:o/r#1".to_owned(), "https://example.com".to_owned()],
                status: TaskStatus::Pending,
                created_at: at(700),
            }
        );
        assert_eq!(open(&dir).tasks(), Ok(vec![added]));
    }

    #[test]
    fn numbers_and_positions_are_sequential_across_reopenings() {
        let dir = TempDir::new().unwrap();
        for (index, title) in ["a", "b", "c"].into_iter().enumerate() {
            let task = open(&dir)
                .append_task(&draft(title), Placement::End, at(1))
                .unwrap();
            assert_eq!(task.id, TaskId(index as u64 + 1));
            assert_eq!(task.position, index + 1);
        }
        let titles: Vec<_> = open(&dir)
            .tasks()
            .unwrap()
            .into_iter()
            .map(|t| (t.position, t.title))
            .collect();
        assert_eq!(
            titles,
            [
                (1, "a".to_owned()),
                (2, "b".to_owned()),
                (3, "c".to_owned())
            ]
        );
    }

    #[test]
    fn a_number_is_never_reused_even_when_the_last_task_is_gone() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        journal
            .append_task(&draft("b"), Placement::End, at(1))
            .unwrap();
        journal
            .connection
            .execute("DELETE FROM tasks WHERE id = 2", [])
            .unwrap();
        assert_eq!(
            journal
                .append_task(&draft("c"), Placement::End, at(1))
                .unwrap()
                .id,
            TaskId(3)
        );
    }

    #[test]
    fn a_task_is_placed_before_or_after_another_and_no_number_changes() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        for title in ["a", "b", "c"] {
            journal
                .append_task(&draft(title), Placement::End, at(1))
                .unwrap();
        }
        let placements = [
            ("before-a", Placement::Before(TaskId(1)), 4, 1),
            ("after-a", Placement::After(TaskId(1)), 5, 3),
            ("before-c", Placement::Before(TaskId(3)), 6, 5),
            ("after-c", Placement::After(TaskId(3)), 7, 7),
        ];
        for (title, placement, id, position) in placements {
            let added = journal
                .append_task(&draft(title), placement, at(1))
                .unwrap();
            assert_eq!(
                (added.id, added.position),
                (TaskId(id), position),
                "{title}"
            );
        }
        let shown: Vec<_> = open(&dir)
            .tasks()
            .unwrap()
            .into_iter()
            .map(|t| (t.position, t.id.0, t.title))
            .collect();
        let expected = [
            (1, 4, "before-a"),
            (2, 1, "a"),
            (3, 5, "after-a"),
            (4, 2, "b"),
            (5, 6, "before-c"),
            (6, 3, "c"),
            (7, 7, "after-c"),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(position, id, title)| (position, id, title.to_owned()))
            .collect();
        assert_eq!(shown, expected);
    }

    #[test]
    fn placing_a_task_next_to_one_that_is_missing_or_cancelled_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        for title in ["a", "b"] {
            journal
                .append_task(&draft(title), Placement::End, at(1))
                .unwrap();
        }
        journal
            .connection
            .execute("UPDATE tasks SET status = 'cancelled' WHERE id = 1", [])
            .unwrap();
        let before = journal.tasks().unwrap();
        for (placement, expected) in [
            (
                Placement::Before(TaskId(9)),
                AppendError::UnknownTask(TaskId(9)),
            ),
            (
                Placement::After(TaskId(9)),
                AppendError::UnknownTask(TaskId(9)),
            ),
            (
                Placement::Before(TaskId(1)),
                AppendError::CancelledTask(TaskId(1)),
            ),
            (
                Placement::After(TaskId(1)),
                AppendError::CancelledTask(TaskId(1)),
            ),
        ] {
            assert_eq!(
                journal.append_task(&draft("x"), placement, at(1)),
                Err(expected)
            );
            assert_eq!(journal.tasks().unwrap(), before);
        }
        let events: i64 = journal
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 2);
    }

    #[test]
    fn a_placed_task_records_where_it_was_placed_in_its_event() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        journal
            .append_task(&draft("b"), Placement::Before(TaskId(1)), at(1))
            .unwrap();
        journal
            .append_task(&draft("c"), Placement::After(TaskId(1)), at(1))
            .unwrap();
        let payloads: Vec<Value> = journal
            .connection
            .prepare("SELECT payload FROM events ORDER BY seq")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect();
        assert_eq!(payloads[0].get("before"), None);
        assert_eq!(payloads[0].get("after"), None);
        assert_eq!(payloads[1]["before"], 1);
        assert_eq!(payloads[2]["after"], 1);
    }

    #[test]
    fn adding_a_task_appends_exactly_one_event_in_the_same_transaction() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(42))
            .unwrap();
        let (count, at, kind, task_id, payload): (i64, i64, String, i64, String) = journal
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
            (count, at, kind.as_str(), task_id),
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
    fn a_failed_add_records_neither_the_task_nor_the_event() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .connection
            .execute_batch("DROP TABLE events")
            .unwrap();
        let error = journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap_err();
        assert!(error.to_string().contains("cannot add the task"), "{error}");
        assert_eq!(journal.tasks(), Ok(vec![]));
    }

    #[test]
    fn a_stored_task_that_cannot_be_understood_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        journal
            .connection
            .execute("UPDATE tasks SET status = 'stuck'", [])
            .unwrap();
        let error = journal.tasks().unwrap_err().to_string();
        assert!(error.contains("task 1"), "{error}");
        assert!(error.contains("status"), "{error}");
    }

    #[test]
    fn a_batch_is_placed_together_in_order_with_one_event_per_task() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        for title in ["a", "b"] {
            journal
                .append_task(&draft(title), Placement::End, at(1))
                .unwrap();
        }
        let batch = [draft("x"), draft("y"), draft("z")];
        let added = journal
            .append_tasks(&batch, Placement::Before(TaskId(2)), at(2))
            .unwrap();
        let placed: Vec<_> = added.iter().map(|t| (t.id.0, t.position)).collect();
        assert_eq!(placed, [(3, 2), (4, 3), (5, 4)]);
        let shown: Vec<_> = journal
            .tasks()
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(shown, ["a", "x", "y", "z", "b"]);
        let events: i64 = journal
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 5);
    }

    #[test]
    fn a_batch_placed_next_to_a_missing_task_records_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        let error = journal
            .append_tasks(
                &[draft("x"), draft("y")],
                Placement::After(TaskId(9)),
                at(2),
            )
            .unwrap_err();
        assert_eq!(error, AppendError::UnknownTask(TaskId(9)));
        assert_eq!(journal.tasks().unwrap().len(), 1);
    }

    #[test]
    fn a_batch_that_fails_part_way_is_rolled_back() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON events
                 WHEN (SELECT COUNT(*) FROM events) >= 2
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
        let error = journal
            .append_tasks(&[draft("x"), draft("y"), draft("z")], Placement::End, at(2))
            .unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
        assert_eq!(journal.tasks().unwrap(), vec![]);
        let events: i64 = journal
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 0);
    }

    fn added_abc(journal: &SqliteJournal) {
        for title in ["a", "b", "c"] {
            journal
                .append_task(&draft(title), Placement::End, at(1))
                .unwrap();
        }
    }

    #[test]
    fn a_cancelled_task_stays_in_its_place_with_its_number_and_records_one_event() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        added_abc(&journal);

        journal.cancel_task(TaskId(2), at(900)).unwrap();

        let shown: Vec<_> = open(&dir)
            .tasks()
            .unwrap()
            .iter()
            .map(|t| (t.position, t.id, t.status))
            .collect();
        assert_eq!(
            shown,
            [
                (1, TaskId(1), TaskStatus::Pending),
                (2, TaskId(2), TaskStatus::Cancelled),
                (3, TaskId(3), TaskStatus::Pending)
            ]
        );
        let (events, kind, task_id, when): (i64, String, i64, i64) = journal
            .connection
            .query_row(
                "SELECT COUNT(*), kind, task_id, at FROM events WHERE seq > 3",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (events, kind.as_str(), task_id, when),
            (1, "task_cancelled", 2, 900)
        );
    }

    #[test]
    fn a_cancelled_tasks_number_is_never_reused_even_at_the_end() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        added_abc(&journal);
        journal.cancel_task(TaskId(3), at(2)).unwrap();
        let next = journal
            .append_task(&draft("d"), Placement::End, at(3))
            .unwrap();
        assert_eq!(next.id, TaskId(4));
    }

    #[test]
    fn cancelling_an_unknown_or_a_cancelled_task_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        added_abc(&journal);
        journal.cancel_task(TaskId(2), at(2)).unwrap();
        let tasks = journal.tasks().unwrap();
        let events = || -> i64 {
            journal
                .connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .unwrap()
        };
        let recorded = events();

        assert_eq!(
            journal.cancel_task(TaskId(9), at(3)),
            Err(CancelError::UnknownTask(TaskId(9)))
        );
        assert_eq!(
            journal.cancel_task(TaskId(2), at(3)),
            Err(CancelError::AlreadyCancelled(TaskId(2)))
        );

        assert_eq!(journal.tasks().unwrap(), tasks);
        assert_eq!(events(), recorded);
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        let number = journal.begin_attempt(TaskId(1), at(50)).unwrap();
        assert_eq!(number, 1);
        assert_eq!(journal.tasks().unwrap()[0].status, TaskStatus::Running);
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        assert_eq!(journal.tasks().unwrap()[0].status, TaskStatus::Running);
    }

    #[test]
    fn a_report_for_an_unknown_task_or_the_wrong_attempt_number_is_refused_and_records_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        journal.cancel_task(TaskId(1), at(2)).unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        journal.begin_attempt(TaskId(1), at(1)).unwrap();

        journal
            .attempt_running(TaskId(1), 1, "echo", at(5))
            .unwrap();

        assert_eq!(journal.tasks().unwrap()[0].status, TaskStatus::Running);
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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

        assert_eq!(journal.tasks().unwrap()[0].status, TaskStatus::Failed);
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        assert_eq!(journal.tasks().unwrap()[0].status, TaskStatus::Running);
    }

    #[test]
    fn end_attempt_for_an_already_ended_attempt_is_refused_and_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        journal.cancel_task(TaskId(1), at(2)).unwrap();
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
        assert_eq!(journal.tasks().unwrap()[0].status, TaskStatus::Cancelled);
    }

    #[test]
    fn running_is_none_until_an_attempt_starts_and_names_its_task_and_attempt_number() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        assert_eq!(journal.running().unwrap(), None);

        journal.begin_attempt(TaskId(1), at(1)).unwrap();
        assert_eq!(journal.running().unwrap(), Some((TaskId(1), 1)));
    }

    #[test]
    fn running_is_none_again_once_the_attempt_ends() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
        assert_eq!(journal.last_attempt(TaskId(1)).unwrap(), None);
        assert_eq!(journal.last_attempt(TaskId(9)).unwrap(), None);
    }

    #[test]
    fn last_attempt_while_running_carries_its_number_start_and_provider_but_no_ending() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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
        journal
            .append_task(&draft("a"), Placement::End, at(1))
            .unwrap();
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

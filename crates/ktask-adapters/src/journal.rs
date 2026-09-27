//! A project's journal in SQLite.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ktask_core::{
    AppendError, Journal, JournalError, Placement, Task, TaskDraft, TaskId, TaskKind, TaskStatus,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

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
                     created_at INTEGER NOT NULL
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
        (
            created_at,
            id,
            serde_json::Value::Object(payload).to_string(),
        ),
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

impl Journal for SqliteJournal {
    fn append_task(
        &self,
        draft: &TaskDraft,
        placement: Placement,
        at: SystemTime,
    ) -> Result<Task, AppendError> {
        let doing = "cannot add the task to the journal";
        // Immediate: take the write lock first, so that reading the order keys and
        // inserting among them cannot interleave with another process adding a task.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
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
        transaction.commit().map_err(|e| failed(doing, e))?;
        Ok(Task {
            id: TaskId(u64::try_from(id).map_err(|e| failed(doing, e))?),
            position: usize::try_from(position).map_err(|e| failed(doing, e))?,
            title: draft.title.clone(),
            body: draft.body.clone(),
            criteria: draft.criteria.clone(),
            kind: draft.kind,
            links: draft.links.clone(),
            status: TaskStatus::Pending,
            created_at: from_seconds(to_seconds(at)),
        })
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
        let payloads: Vec<serde_json::Value> = journal
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
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
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

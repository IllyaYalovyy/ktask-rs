//! A project's journal in SQLite.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ktask_core::{Journal, JournalError, Task, TaskDraft, TaskId, TaskKind, TaskStatus};
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// How long a writer waits for another process's transaction to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A project's journal, kept in a SQLite database file.
///
/// `events` is the journal proper: one row per event, appended and never changed. `tasks` is
/// the projection of it, changed only in the transaction that appends the event. Task
/// numbers come from `AUTOINCREMENT`, which never hands out a number twice.
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

/// Inserts `draft` as the last task and records the event, inside `transaction`. Returns
/// the new task's number.
fn insert(
    transaction: &Transaction<'_>,
    draft: &TaskDraft,
    created_at: i64,
) -> Result<i64, rusqlite::Error> {
    let criteria = serde_json::json!(draft.criteria).to_string();
    let links = serde_json::json!(draft.links).to_string();
    transaction.execute(
        "INSERT INTO tasks
             (order_key, title, body, criteria, kind, links, status, created_at)
         VALUES
             ((SELECT COALESCE(MAX(order_key), 0) + 1 FROM tasks),
              ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
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
    let payload = serde_json::json!({
        "title": draft.title,
        "body": draft.body,
        "criteria": draft.criteria,
        "kind": draft.kind.as_str(),
        "links": draft.links,
    })
    .to_string();
    transaction.execute(
        "INSERT INTO events (at, kind, task_id, payload) VALUES (?1, 'task_added', ?2, ?3)",
        (created_at, id, payload),
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
    fn append_task(&self, draft: &TaskDraft, at: SystemTime) -> Result<Task, JournalError> {
        let doing = "cannot add the task to the journal";
        // Immediate: take the write lock first, so that reading the last order key and
        // inserting after it cannot interleave with another process adding a task.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(|e| failed(doing, e))?;
        let id = insert(&transaction, draft, to_seconds(at)).map_err(|e| failed(doing, e))?;
        let position: i64 = transaction
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
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
        let added = open(&dir).append_task(&draft("t"), at(700)).unwrap();
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
            let task = open(&dir).append_task(&draft(title), at(1)).unwrap();
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
        journal.append_task(&draft("a"), at(1)).unwrap();
        journal.append_task(&draft("b"), at(1)).unwrap();
        journal
            .connection
            .execute("DELETE FROM tasks WHERE id = 2", [])
            .unwrap();
        assert_eq!(
            journal.append_task(&draft("c"), at(1)).unwrap().id,
            TaskId(3)
        );
    }

    #[test]
    fn adding_a_task_appends_exactly_one_event_in_the_same_transaction() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal.append_task(&draft("a"), at(42)).unwrap();
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
        let error = journal.append_task(&draft("a"), at(1)).unwrap_err();
        assert!(error.to_string().contains("cannot add the task"), "{error}");
        assert_eq!(journal.tasks(), Ok(vec![]));
    }

    #[test]
    fn a_stored_task_that_cannot_be_understood_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let journal = open(&dir);
        journal.append_task(&draft("a"), at(1)).unwrap();
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

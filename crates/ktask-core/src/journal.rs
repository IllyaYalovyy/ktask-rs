//! Event journal for storing and retrieving task events.

use crate::{Error, Event, EventKind, EventSeq, Project, Result, TaskId};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// The current schema version.
const SCHEMA_VERSION: i32 = 1;

/// Event journal backed by SQLite.
#[derive(Debug)]
pub struct Journal {
    #[doc(hidden)]
    pub conn: Connection,
}

impl Journal {
    /// Open or create a journal at the given path.
    ///
    /// Creates the database file and schema if it doesn't exist, or
    /// opens an existing journal. The operation is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or created,
    /// if the schema cannot be initialized, or if the database is
    /// from a future schema version.
    pub fn open(path: &Path) -> Result<Journal> {
        let conn = Connection::open(path)?;

        // Configure WAL mode and synchronous settings
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;

        // Initialize schema
        Self::init_schema(&conn)?;

        Ok(Journal { conn })
    }

    /// Get the standard journal path for a state directory.
    ///
    /// Returns `<state_dir>/journal.db`.
    #[must_use]
    pub fn journal_path(state_dir: &Path) -> PathBuf {
        state_dir.join("journal.db")
    }

    /// Open a journal for the given project.
    ///
    /// Calls `journal_path` to determine the location and `open` to
    /// open or create it.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal cannot be opened.
    pub fn open_for(project: &Project) -> Result<Journal> {
        let path = Self::journal_path(&project.state_dir);
        Self::open(&path)
    }

    /// Append an event to the journal.
    ///
    /// Appends an event with the given `task_id` and `kind` to the journal,
    /// storing the event's discriminant in the `kind` column and serializing
    /// the payload as JSON. The timestamp is automatically set to the current
    /// time in UTC. The insert is wrapped in a transaction to ensure atomicity.
    ///
    /// # Errors
    ///
    /// Returns an error if the event cannot be serialized, inserted, or if
    /// the transaction cannot be committed.
    pub fn append(&mut self, task_id: Option<TaskId>, kind: &EventKind) -> Result<EventSeq> {
        // Serialize the payload
        let payload = serde_json::to_string(kind)?;

        // Get the current timestamp in UTC
        let ts = OffsetDateTime::now_utc();
        let ts_str = ts
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| Error::Corrupt {
                detail: "Failed to format timestamp".to_string(),
                seq: None,
            })?;

        // Get the discriminant
        let kind_str = kind.discriminant();

        // Convert task_id to Option<i64> for SQLite
        let task_id_val = task_id.map(|id| i64::from(id.get()));

        // Insert in a transaction
        let tx = self.conn.transaction()?;

        // Insert the event
        tx.execute(
            "INSERT INTO events (ts, task_id, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![ts_str, task_id_val, kind_str, payload],
        )?;

        // Get the last inserted row ID (the sequence number)
        let seq = tx.last_insert_rowid().cast_unsigned();

        // Commit the transaction
        tx.commit()?;

        Ok(EventSeq::new(seq))
    }

    /// Read all events from the journal in sequence order.
    ///
    /// Returns all stored events ordered by sequence number ascending.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or events cannot be deserialized.
    pub fn events(&self) -> Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, ts, task_id, payload FROM events ORDER BY seq ASC")?;

        let events = stmt.query_map([], |row| {
            let seq_val: i64 = row.get(0)?;
            let ts_str: String = row.get(1)?;
            let task_id_val: Option<i64> = row.get(2)?;
            let payload_str: String = row.get(3)?;

            Ok((seq_val.cast_unsigned(), ts_str, task_id_val, payload_str))
        })?;

        let mut result = Vec::new();
        for event_result in events {
            let (seq_val, ts_str, task_id_val, payload_str) = event_result?;

            // Parse timestamp
            let ts = OffsetDateTime::parse(&ts_str, &time::format_description::well_known::Rfc3339)
                .map_err(|_| Error::Corrupt {
                    detail: format!("Failed to parse timestamp: {ts_str}"),
                    seq: Some(seq_val),
                })?;

            // Deserialize kind from payload
            let kind: EventKind =
                serde_json::from_str(&payload_str).map_err(|_| Error::Corrupt {
                    detail: format!("Failed to deserialize payload for seq {seq_val}"),
                    seq: Some(seq_val),
                })?;

            // Convert task_id (stored as i64 in SQLite, originally u32)
            let task_id = task_id_val.map(|id| {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let task_id_u32 = id as u32;
                TaskId::new(task_id_u32)
            });

            result.push(Event {
                seq: EventSeq::new(seq_val),
                ts,
                task_id,
                kind,
            });
        }

        Ok(result)
    }

    /// Read events for a specific task in sequence order.
    ///
    /// Returns all events associated with the given task, ordered by
    /// sequence number ascending.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or events cannot be deserialized.
    pub fn events_for(&self, task: TaskId) -> Result<Vec<Event>> {
        let task_val = i64::from(task.get());
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, task_id, payload FROM events WHERE task_id = ?1 ORDER BY seq ASC",
        )?;

        let events = stmt.query_map([task_val], |row| {
            let seq_val: i64 = row.get(0)?;
            let ts_str: String = row.get(1)?;
            let task_id_val: Option<i64> = row.get(2)?;
            let payload_str: String = row.get(3)?;

            Ok((seq_val.cast_unsigned(), ts_str, task_id_val, payload_str))
        })?;

        let mut result = Vec::new();
        for event_result in events {
            let (seq_val, ts_str, task_id_val, payload_str) = event_result?;

            // Parse timestamp
            let ts = OffsetDateTime::parse(&ts_str, &time::format_description::well_known::Rfc3339)
                .map_err(|_| Error::Corrupt {
                    detail: format!("Failed to parse timestamp: {ts_str}"),
                    seq: Some(seq_val),
                })?;

            // Deserialize kind from payload
            let kind: EventKind =
                serde_json::from_str(&payload_str).map_err(|_| Error::Corrupt {
                    detail: format!("Failed to deserialize payload for seq {seq_val}"),
                    seq: Some(seq_val),
                })?;

            // Convert task_id (stored as i64 in SQLite, originally u32)
            let task_id = task_id_val.map(|id| {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let task_id_u32 = id as u32;
                TaskId::new(task_id_u32)
            });

            result.push(Event {
                seq: EventSeq::new(seq_val),
                ts,
                task_id,
                kind,
            });
        }

        Ok(result)
    }

    /// Stream events since a specific sequence number without collecting into memory.
    ///
    /// Calls the provided callback function for each event with sequence number
    /// greater than the given sequence, in ascending order. Uses a prepared statement
    /// and row iteration to avoid loading all events into memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails, events cannot be deserialized,
    /// or if the callback returns an error.
    pub fn for_each_event<F>(&self, from: EventSeq, f: &mut F) -> Result<()>
    where
        F: FnMut(Event) -> Result<()>,
    {
        let from_val = from.get().cast_signed();
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, task_id, payload FROM events WHERE seq > ?1 ORDER BY seq ASC",
        )?;

        let events = stmt.query_map([from_val], |row| {
            let seq_val: i64 = row.get(0)?;
            let ts_str: String = row.get(1)?;
            let task_id_val: Option<i64> = row.get(2)?;
            let payload_str: String = row.get(3)?;

            Ok((seq_val.cast_unsigned(), ts_str, task_id_val, payload_str))
        })?;

        for event_result in events {
            let (seq_val, ts_str, task_id_val, payload_str) = event_result?;

            // Parse timestamp
            let ts = OffsetDateTime::parse(&ts_str, &time::format_description::well_known::Rfc3339)
                .map_err(|_| Error::Corrupt {
                    detail: format!("Failed to parse timestamp: {ts_str}"),
                    seq: Some(seq_val),
                })?;

            // Deserialize kind from payload
            let kind: EventKind =
                serde_json::from_str(&payload_str).map_err(|_| Error::Corrupt {
                    detail: format!("Failed to deserialize payload for seq {seq_val}"),
                    seq: Some(seq_val),
                })?;

            // Convert task_id (stored as i64 in SQLite, originally u32)
            let task_id = task_id_val.map(|id| {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let task_id_u32 = id as u32;
                TaskId::new(task_id_u32)
            });

            let event = Event {
                seq: EventSeq::new(seq_val),
                ts,
                task_id,
                kind,
            };

            f(event)?;
        }

        Ok(())
    }

    /// Read events since a specific sequence number in sequence order.
    ///
    /// Returns all events with sequence number greater than the given
    /// sequence, ordered by sequence number ascending.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or events cannot be deserialized.
    pub fn events_since(&self, seq: EventSeq) -> Result<Vec<Event>> {
        let seq_val = seq.get().cast_signed();
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, task_id, payload FROM events WHERE seq > ?1 ORDER BY seq ASC",
        )?;

        let events = stmt.query_map([seq_val], |row| {
            let seq_val: i64 = row.get(0)?;
            let ts_str: String = row.get(1)?;
            let task_id_val: Option<i64> = row.get(2)?;
            let payload_str: String = row.get(3)?;

            Ok((seq_val.cast_unsigned(), ts_str, task_id_val, payload_str))
        })?;

        let mut result = Vec::new();
        for event_result in events {
            let (seq_val, ts_str, task_id_val, payload_str) = event_result?;

            // Parse timestamp
            let ts = OffsetDateTime::parse(&ts_str, &time::format_description::well_known::Rfc3339)
                .map_err(|_| Error::Corrupt {
                    detail: format!("Failed to parse timestamp: {ts_str}"),
                    seq: Some(seq_val),
                })?;

            // Deserialize kind from payload
            let kind: EventKind =
                serde_json::from_str(&payload_str).map_err(|_| Error::Corrupt {
                    detail: format!("Failed to deserialize payload for seq {seq_val}"),
                    seq: Some(seq_val),
                })?;

            // Convert task_id (stored as i64 in SQLite, originally u32)
            let task_id = task_id_val.map(|id| {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let task_id_u32 = id as u32;
                TaskId::new(task_id_u32)
            });

            result.push(Event {
                seq: EventSeq::new(seq_val),
                ts,
                task_id,
                kind,
            });
        }

        Ok(result)
    }

    /// Store tasks in the queue.
    ///
    /// Inserts a parsed plan into the tasks table in document order.
    /// Importing a plan into a non-empty queue is an error naming the existing task count.
    /// Task status is not stored here; it is derived from the journal.
    ///
    /// # Errors
    ///
    /// Returns an error if the queue is non-empty or if the insert fails.
    pub fn put_tasks(&mut self, tasks: &[crate::Task]) -> Result<()> {
        // Check if queue is non-empty
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))?;

        if count > 0 {
            return Err(Error::Policy {
                detail: format!("Cannot import plan: queue already has {count} task(s)"),
                paths: vec![],
            });
        }

        // Insert all tasks
        let ts = OffsetDateTime::now_utc();
        let ts_str = ts
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| Error::Corrupt {
                detail: "Failed to format timestamp".to_string(),
                seq: None,
            })?;

        for task in tasks {
            let id = i64::from(task.id.get());
            let title = task.title().to_string();

            self.conn.execute(
                "INSERT INTO tasks (id, title, outcome, done_when, verify, refs, protocol, body, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    id,
                    title,
                    &task.outcome,
                    &task.done_when,
                    &task.verify,
                    &task.refs,
                    None::<String>,
                    &task.body,
                    ts_str,
                ],
            )?;
        }

        Ok(())
    }

    /// Read tasks from the queue.
    ///
    /// Returns all stored tasks ordered by id.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn tasks(&self) -> Result<Vec<crate::Task>> {
        use crate::{Task, TaskStatus};

        let mut stmt = self.conn.prepare(
            "SELECT id, outcome, done_when, verify, refs, body FROM tasks ORDER BY id ASC",
        )?;

        let tasks = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let outcome: String = row.get(1)?;
            let done_when: String = row.get(2)?;
            let verify: String = row.get(3)?;
            let refs: String = row.get(4)?;
            let body: String = row.get(5)?;

            Ok((id, outcome, done_when, verify, refs, body))
        })?;

        let mut result = Vec::new();
        for task_result in tasks {
            let (id, outcome, done_when, verify, refs, body) = task_result?;

            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let task_id_u32 = id as u32;

            let task = Task {
                id: TaskId::new(task_id_u32),
                status: TaskStatus::Pending,
                body,
                outcome,
                done_when,
                verify,
                refs,
            };

            result.push(task);
        }

        Ok(result)
    }

    /// Initialize or validate the schema.
    ///
    /// Creates all tables and index if they don't exist, validates the
    /// schema version, and stores it if this is the first time.
    fn init_schema(conn: &Connection) -> Result<()> {
        // Create the meta table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            [],
        )?;

        // Check schema version
        let stored_version: std::result::Result<String, rusqlite::Error> = conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        );

        match stored_version {
            Ok(version_str) => {
                // Schema already exists, validate version
                let version: i32 = version_str.parse().unwrap_or(0);
                if version > SCHEMA_VERSION {
                    return Err(Error::Corrupt {
                        detail: format!(
                            "Journal schema version {version} is not supported (current: {SCHEMA_VERSION})",
                        ),
                        seq: None,
                    });
                }
                // If we ever need migration logic for older versions, add it here
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // First time initialization: create all tables and store version
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS events (
                        seq      INTEGER PRIMARY KEY AUTOINCREMENT,
                        ts       TEXT    NOT NULL,
                        task_id  INTEGER,
                        kind     TEXT    NOT NULL,
                        payload  TEXT    NOT NULL
                    )",
                    [],
                )?;

                // Create triggers to enforce append-only semantics
                conn.execute(
                    "CREATE TRIGGER IF NOT EXISTS trigger_prevent_update_events
                        BEFORE UPDATE ON events
                        BEGIN
                            SELECT RAISE(ABORT, 'journal is append-only: updates not allowed');
                        END",
                    [],
                )?;

                conn.execute(
                    "CREATE TRIGGER IF NOT EXISTS trigger_prevent_delete_events
                        BEFORE DELETE ON events
                        BEGIN
                            SELECT RAISE(ABORT, 'journal is append-only: deletes not allowed');
                        END",
                    [],
                )?;

                conn.execute(
                    "CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id, seq)",
                    [],
                )?;

                conn.execute(
                    "CREATE TABLE IF NOT EXISTS tasks (
                        id         INTEGER PRIMARY KEY,
                        title      TEXT    NOT NULL,
                        outcome    TEXT    NOT NULL,
                        done_when  TEXT    NOT NULL,
                        verify     TEXT    NOT NULL,
                        refs       TEXT    NOT NULL,
                        protocol   TEXT,
                        body       TEXT    NOT NULL,
                        added_at   TEXT    NOT NULL
                    )",
                    [],
                )?;

                conn.execute(
                    "CREATE TABLE IF NOT EXISTS task_state (
                        task_id    INTEGER PRIMARY KEY,
                        state_json TEXT    NOT NULL,
                        updated_at TEXT    NOT NULL
                    )",
                    [],
                )?;

                // Store schema version
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    rusqlite::params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Err(e) => {
                // Some other database error
                return Err(Error::Database(e));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_journal_file() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        assert!(journal_path.exists());
        drop(journal);
    }

    #[test]
    fn open_twice_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal1 = Journal::open(&journal_path).unwrap();
        drop(journal1);

        // Opening again should not error
        let journal2 = Journal::open(&journal_path).unwrap();
        drop(journal2);
    }

    #[test]
    fn schema_version_stored_in_meta() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let conn = &journal.conn;

        let version: i32 = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| {
                    let val: String = row.get(0)?;
                    Ok(val.parse().unwrap_or(0))
                },
            )
            .expect("schema_version should exist");

        assert_eq!(version, SCHEMA_VERSION);
        drop(journal);
    }

    #[test]
    fn tables_created_on_first_open() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let conn = &journal.conn;

        // Check that all tables exist
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert!(tables.contains(&"meta".to_string()));
        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"tasks".to_string()));
        assert!(tables.contains(&"task_state".to_string()));
        drop(journal);
    }

    #[test]
    fn index_created_on_first_open() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let conn = &journal.conn;

        // Check that the index exists
        let indices: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name='idx_events_task'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(indices.len(), 1);
        drop(journal);
    }

    #[test]
    fn future_schema_version_is_error() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        // Create a journal with current version
        {
            let journal = Journal::open(&journal_path).unwrap();
            let conn = &journal.conn;

            // Update the schema_version to a future version
            conn.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                rusqlite::params![(SCHEMA_VERSION + 1).to_string()],
            )
            .unwrap();
            drop(journal);
        }

        // Try to open it again - should fail with a clear error
        let result = Journal::open(&journal_path);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::Corrupt { detail, .. } => {
                assert!(detail.contains("schema version"));
                assert!(detail.contains("not supported"));
            }
            _ => panic!("expected Corrupt error for future schema version"),
        }
    }

    #[test]
    fn journal_path_returns_correct_path() {
        let state_dir = PathBuf::from("/tmp/state");
        let path = Journal::journal_path(&state_dir);
        assert_eq!(path, PathBuf::from("/tmp/state/journal.db"));
    }

    #[test]
    fn open_for_uses_correct_path() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(repo_path)
            .output()
            .expect("git init failed");

        let project = crate::project::register(repo_path).unwrap();
        let journal = Journal::open_for(&project).unwrap();

        let expected_path = Journal::journal_path(&project.state_dir);
        assert!(expected_path.exists());
        drop(journal);
    }

    #[test]
    fn wal_mode_enabled() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let conn = &journal.conn;

        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("should get journal_mode");

        assert_eq!(journal_mode.to_lowercase(), "wal");
        drop(journal);
    }

    #[test]
    fn synchronous_full_enabled() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let conn = &journal.conn;

        let synchronous: i32 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("should get synchronous");

        // FULL = 2
        assert_eq!(synchronous, 2);
        drop(journal);
    }

    #[test]
    fn append_returns_event_seq() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        let seq = journal.append(None, &kind).unwrap();
        assert_eq!(seq, EventSeq::new(1));
        drop(journal);
    }

    #[test]
    fn append_strictly_increasing_sequences() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        let seq1 = journal.append(None, &kind).unwrap();
        let seq2 = journal.append(None, &kind).unwrap();
        let seq3 = journal.append(None, &kind).unwrap();

        assert_eq!(seq1, EventSeq::new(1));
        assert_eq!(seq2, EventSeq::new(2));
        assert_eq!(seq3, EventSeq::new(3));
        drop(journal);
    }

    #[test]
    fn append_with_task_id() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        let task_id = TaskId::new(42);
        let seq = journal.append(Some(task_id), &kind).unwrap();
        assert_eq!(seq, EventSeq::new(1));

        // Verify task_id was stored
        let stored_task_id: i64 = journal
            .conn
            .query_row("SELECT task_id FROM events WHERE seq = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored_task_id, 42);
        drop(journal);
    }

    #[test]
    fn append_without_task_id() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::PreflightStarted;

        let seq = journal.append(None, &kind).unwrap();
        assert_eq!(seq, EventSeq::new(1));

        // Verify task_id is NULL
        let task_id: Option<i64> = journal
            .conn
            .query_row("SELECT task_id FROM events WHERE seq = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(task_id, None);
        drop(journal);
    }

    #[test]
    fn append_sequence_survives_reopen() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        // First session: append some events
        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        let seq1 = journal.append(None, &kind).unwrap();
        let seq2 = journal.append(None, &kind).unwrap();
        assert_eq!(seq1, EventSeq::new(1));
        assert_eq!(seq2, EventSeq::new(2));
        drop(journal);

        // Second session: reopen and continue appending
        let mut journal = Journal::open(&journal_path).unwrap();
        let seq3 = journal.append(None, &kind).unwrap();
        let seq4 = journal.append(None, &kind).unwrap();
        assert_eq!(seq3, EventSeq::new(3));
        assert_eq!(seq4, EventSeq::new(4));
        drop(journal);
    }

    #[test]
    fn append_stores_discriminant_and_payload() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        journal.append(None, &kind).unwrap();

        // Verify discriminant and payload were stored
        let (stored_kind, stored_payload): (String, String) = journal
            .conn
            .query_row(
                "SELECT kind, payload FROM events WHERE seq = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(stored_kind, "TaskQueued");

        // Verify payload is valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&stored_payload).unwrap();
        assert_eq!(parsed["kind"], "TaskQueued");
        assert_eq!(parsed["title"], "Test task");

        drop(journal);
    }

    #[test]
    fn append_stores_timestamp() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let before = OffsetDateTime::now_utc();
        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::PreflightStarted;

        journal.append(None, &kind).unwrap();
        let after = OffsetDateTime::now_utc();

        // Verify timestamp was stored
        let ts_str: String = journal
            .conn
            .query_row("SELECT ts FROM events WHERE seq = 1", [], |row| row.get(0))
            .unwrap();

        // Parse the timestamp
        let ts =
            OffsetDateTime::parse(&ts_str, &time::format_description::well_known::Rfc3339).unwrap();

        // Verify timestamp is within reasonable bounds (before and after)
        assert!(ts >= before);
        assert!(ts <= after);

        drop(journal);
    }

    #[test]
    fn journal_rejects_update() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        journal.append(None, &kind).unwrap();

        // Verify the original row exists
        let original_kind: String = journal
            .conn
            .query_row("SELECT kind FROM events WHERE seq = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(original_kind, "TaskQueued");

        // Attempt to update the row - should fail
        let result = journal
            .conn
            .execute("UPDATE events SET kind = 'Modified' WHERE seq = 1", []);
        assert!(result.is_err(), "UPDATE should be rejected by trigger");

        // Verify the row is unchanged
        let final_kind: String = journal
            .conn
            .query_row("SELECT kind FROM events WHERE seq = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            final_kind, "TaskQueued",
            "Event should remain unchanged after failed UPDATE"
        );

        drop(journal);
    }

    #[test]
    fn journal_rejects_delete() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        journal.append(None, &kind).unwrap();

        // Verify the row exists
        let count_before: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 1);

        // Attempt to delete the row - should fail
        let result = journal.conn.execute("DELETE FROM events WHERE seq = 1", []);
        assert!(result.is_err(), "DELETE should be rejected by trigger");

        // Verify the row still exists
        let count_after: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 1, "Event should not be deleted");

        drop(journal);
    }

    #[test]
    fn events_empty_journal() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let events = journal.events().unwrap();

        assert_eq!(events.len(), 0);
        drop(journal);
    }

    #[test]
    fn events_single_event() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        journal.append(None, &kind).unwrap();

        let events = journal.events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, EventSeq::new(1));
        assert_eq!(events[0].task_id, None);
        assert_eq!(events[0].kind, kind);

        drop(journal);
    }

    #[test]
    fn events_many_events() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();

        // Append multiple events
        for i in 1..=10 {
            let kind = EventKind::TaskQueued {
                title: format!("Task {i}"),
            };
            journal.append(None, &kind).unwrap();
        }

        let events = journal.events().unwrap();
        assert_eq!(events.len(), 10);

        // Verify ordering by sequence
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.seq, EventSeq::new((i + 1) as u64));
        }

        drop(journal);
    }

    #[test]
    fn events_ordered_by_sequence() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();

        // Append events with different task IDs to verify ordering is by seq, not insertion time
        let kind1 = EventKind::TaskQueued {
            title: "Task 1".to_string(),
        };
        let kind2 = EventKind::PreflightStarted;
        let kind3 = EventKind::TaskQueued {
            title: "Task 2".to_string(),
        };

        journal.append(None, &kind1).unwrap();
        journal.append(None, &kind2).unwrap();
        journal.append(None, &kind3).unwrap();

        let events = journal.events().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, EventSeq::new(1));
        assert_eq!(events[1].seq, EventSeq::new(2));
        assert_eq!(events[2].seq, EventSeq::new(3));

        drop(journal);
    }

    #[test]
    fn events_for_empty_task() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        // Append an event without a task_id
        journal.append(None, &kind).unwrap();

        // Query for a specific task that has no events
        let events = journal.events_for(TaskId::new(42)).unwrap();
        assert_eq!(events.len(), 0);

        drop(journal);
    }

    #[test]
    fn events_for_single_task() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        let task_id = TaskId::new(42);
        journal.append(Some(task_id), &kind).unwrap();

        let events = journal.events_for(task_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, EventSeq::new(1));
        assert_eq!(events[0].task_id, Some(task_id));
        assert_eq!(events[0].kind, kind);

        drop(journal);
    }

    #[test]
    fn events_for_multiple_tasks() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind1 = EventKind::TaskQueued {
            title: "Task 1".to_string(),
        };
        let kind2 = EventKind::PreflightStarted;

        let task1 = TaskId::new(1);
        let task2 = TaskId::new(2);

        // Interleave events from different tasks
        journal.append(Some(task1), &kind1).unwrap(); // seq 1
        journal.append(Some(task2), &kind2).unwrap(); // seq 2
        journal.append(Some(task1), &kind2).unwrap(); // seq 3
        journal.append(Some(task2), &kind1).unwrap(); // seq 4
        journal.append(Some(task1), &kind1).unwrap(); // seq 5

        let events_task1 = journal.events_for(task1).unwrap();
        let events_task2 = journal.events_for(task2).unwrap();

        assert_eq!(events_task1.len(), 3);
        assert_eq!(events_task2.len(), 2);

        // Verify ordering for task1
        assert_eq!(events_task1[0].seq, EventSeq::new(1));
        assert_eq!(events_task1[1].seq, EventSeq::new(3));
        assert_eq!(events_task1[2].seq, EventSeq::new(5));

        // Verify ordering for task2
        assert_eq!(events_task2[0].seq, EventSeq::new(2));
        assert_eq!(events_task2[1].seq, EventSeq::new(4));

        drop(journal);
    }

    #[test]
    fn events_since_empty() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();

        let events = journal.events_since(EventSeq::new(0)).unwrap();
        assert_eq!(events.len(), 0);

        drop(journal);
    }

    #[test]
    fn events_since_single_event() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };

        journal.append(None, &kind).unwrap();

        // Query for events since seq 0 (should return the event at seq 1)
        let events = journal.events_since(EventSeq::new(0)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, EventSeq::new(1));

        // Query for events since seq 1 (should be empty)
        let events = journal.events_since(EventSeq::new(1)).unwrap();
        assert_eq!(events.len(), 0);

        drop(journal);
    }

    #[test]
    fn events_since_multiple_events() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();

        // Append 5 events
        for i in 1..=5 {
            let kind = EventKind::TaskQueued {
                title: format!("Task {i}"),
            };
            journal.append(None, &kind).unwrap();
        }

        // Query for events since seq 2 (should return seq 3, 4, 5)
        let events = journal.events_since(EventSeq::new(2)).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, EventSeq::new(3));
        assert_eq!(events[1].seq, EventSeq::new(4));
        assert_eq!(events[2].seq, EventSeq::new(5));

        // Query for events since seq 4 (should return seq 5)
        let events = journal.events_since(EventSeq::new(4)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, EventSeq::new(5));

        // Query for events since seq 5 (should be empty)
        let events = journal.events_since(EventSeq::new(5)).unwrap();
        assert_eq!(events.len(), 0);

        drop(journal);
    }

    #[test]
    fn events_roundtrip_complex_kind() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::PreflightPassed {
            base_sha: "abc123def456".to_string(),
        };

        journal.append(None, &kind).unwrap();

        let events = journal.events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, kind);

        drop(journal);
    }

    #[test]
    fn journal_tasks_empty_queue() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let tasks = journal.tasks().unwrap();

        assert_eq!(tasks.len(), 0);
        drop(journal);
    }

    #[test]
    fn journal_tasks_roundtrip_single_task() {
        use crate::{TaskStatus, task};

        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let plan = r"## Fix the bug

**Outcome:** The bug is fixed

**Done-when:** Tests pass

**Verify:** cargo test

**Refs:** Issue #123
";
        let parsed = task::parse_plan(plan).unwrap();
        assert_eq!(parsed.len(), 1);

        // Store tasks
        let mut journal = Journal::open(&journal_path).unwrap();
        journal.put_tasks(&parsed).unwrap();
        drop(journal);

        // Read tasks back
        let journal = Journal::open(&journal_path).unwrap();
        let stored = journal.tasks().unwrap();

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, parsed[0].id);
        assert_eq!(stored[0].outcome, parsed[0].outcome);
        assert_eq!(stored[0].done_when, parsed[0].done_when);
        assert_eq!(stored[0].verify, parsed[0].verify);
        assert_eq!(stored[0].refs, parsed[0].refs);
        assert_eq!(stored[0].body, parsed[0].body);
        assert_eq!(stored[0].status, TaskStatus::Pending);
        drop(journal);
    }

    #[test]
    fn journal_tasks_roundtrip_multiple_tasks() {
        use crate::task;

        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let plan = r"## First task

**Outcome:** First outcome

**Done-when:** When first is done

**Verify:** cargo test

**Refs:** Ref 1

## Second task

**Outcome:** Second outcome

**Done-when:** When second is done

**Verify:** cargo test

**Refs:** Ref 2

## Third task

**Outcome:** Third outcome

**Done-when:** When third is done

**Verify:** cargo test

**Refs:** Ref 3
";
        let parsed = task::parse_plan(plan).unwrap();
        assert_eq!(parsed.len(), 3);

        // Store tasks
        let mut journal = Journal::open(&journal_path).unwrap();
        journal.put_tasks(&parsed).unwrap();
        drop(journal);

        // Read tasks back
        let journal = Journal::open(&journal_path).unwrap();
        let stored = journal.tasks().unwrap();

        assert_eq!(stored.len(), 3);
        for i in 0..3 {
            assert_eq!(stored[i].id, parsed[i].id);
            assert_eq!(stored[i].outcome, parsed[i].outcome);
            assert_eq!(stored[i].done_when, parsed[i].done_when);
            assert_eq!(stored[i].verify, parsed[i].verify);
            assert_eq!(stored[i].refs, parsed[i].refs);
            assert_eq!(stored[i].body, parsed[i].body);
        }
        drop(journal);
    }

    #[test]
    fn journal_put_tasks_into_nonempty_queue_errors() {
        use crate::task;

        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let plan1 = r"## First batch

**Outcome:** First

**Done-when:** Done

**Verify:** Test

**Refs:** Ref
";
        let plan2 = r"## Second batch

**Outcome:** Second

**Done-when:** Done

**Verify:** Test

**Refs:** Ref
";
        let parsed1 = task::parse_plan(plan1).unwrap();
        let parsed2 = task::parse_plan(plan2).unwrap();

        // Store first batch
        let mut journal = Journal::open(&journal_path).unwrap();
        journal.put_tasks(&parsed1).unwrap();
        drop(journal);

        // Try to store second batch - should fail
        let mut journal = Journal::open(&journal_path).unwrap();
        let result = journal.put_tasks(&parsed2);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::Policy { detail, .. } => {
                assert!(detail.contains("Cannot import plan"));
                assert!(detail.contains("queue already has"));
                assert!(detail.contains("1 task(s)"));
            }
            _ => panic!("expected Policy error"),
        }
        drop(journal);
    }

    #[test]
    fn journal_put_tasks_with_multiple_existing_tasks() {
        use crate::task;

        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let plan_many = r"## Task 1
**Outcome:** O1
**Done-when:** D1
**Verify:** V1
**Refs:** R1

## Task 2
**Outcome:** O2
**Done-when:** D2
**Verify:** V2
**Refs:** R2

## Task 3
**Outcome:** O3
**Done-when:** D3
**Verify:** V3
**Refs:** R3
";
        let new_plan = r"## New task
**Outcome:** New
**Done-when:** Done
**Verify:** Test
**Refs:** Ref
";

        let parsed_many = task::parse_plan(plan_many).unwrap();
        let parsed_new = task::parse_plan(new_plan).unwrap();

        // Store first batch with 3 tasks
        let mut journal = Journal::open(&journal_path).unwrap();
        journal.put_tasks(&parsed_many).unwrap();
        drop(journal);

        // Try to store new plan - should fail with count of 3
        let mut journal = Journal::open(&journal_path).unwrap();
        let result = journal.put_tasks(&parsed_new);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::Policy { detail, .. } => {
                assert!(detail.contains("3 task(s)"));
            }
            _ => panic!("expected Policy error"),
        }
        drop(journal);
    }

    #[test]
    fn for_each_event_streams_without_collecting() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();

        // Append 10,000 events
        for i in 1..=10_000 {
            let kind = EventKind::TaskQueued {
                title: format!("Task {i}"),
            };
            journal.append(None, &kind).unwrap();
        }

        drop(journal);

        // Reopen and use for_each_event to count them without collecting
        let journal = Journal::open(&journal_path).unwrap();
        let mut count = 0usize;

        journal
            .for_each_event(EventSeq::new(0), &mut |_event| {
                count += 1;
                Ok(())
            })
            .unwrap();

        assert_eq!(count, 10_000);
        drop(journal);
    }
}

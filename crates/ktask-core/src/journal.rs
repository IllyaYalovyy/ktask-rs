//! Event journal for storing and retrieving task events.

use crate::{Error, Event, EventKind, EventSeq, Project, Result, TaskId, TaskState};
use rusqlite::Connection;
use std::collections::BTreeMap;
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

    /// Store task state in the journal.
    ///
    /// Inserts or updates the state for a task, overwriting any existing state.
    /// State is serialized as JSON and stored with a timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error if the state cannot be serialized or if the update fails.
    pub fn put_state(&mut self, task: TaskId, state: &TaskState) -> Result<()> {
        let state_json = serde_json::to_string(state)?;

        let ts = OffsetDateTime::now_utc();
        let ts_str = ts
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| Error::Corrupt {
                detail: "Failed to format timestamp".to_string(),
                seq: None,
            })?;

        let task_id_val = i64::from(task.get());

        self.conn.execute(
            "INSERT OR REPLACE INTO task_state (task_id, state_json, updated_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![task_id_val, state_json, ts_str],
        )?;

        Ok(())
    }

    /// Retrieve task state from the journal.
    ///
    /// Returns the stored state for a task, or None if no state has been stored.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or if the state cannot be deserialized.
    pub fn get_state(&self, task: TaskId) -> Result<Option<TaskState>> {
        let task_id_val = i64::from(task.get());

        let result = self.conn.query_row(
            "SELECT state_json FROM task_state WHERE task_id = ?1",
            rusqlite::params![task_id_val],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(state_json) => {
                let state: TaskState = serde_json::from_str(&state_json)?;
                Ok(Some(state))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Database(e)),
        }
    }

    /// Retrieve all task states from the journal.
    ///
    /// Returns a map of all stored task states ordered by task ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or if any state cannot be deserialized.
    pub fn all_states(&self) -> Result<BTreeMap<TaskId, TaskState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_id, state_json FROM task_state ORDER BY task_id ASC")?;

        let states = stmt.query_map([], |row| {
            let task_id: i64 = row.get(0)?;
            let state_json: String = row.get(1)?;

            Ok((task_id, state_json))
        })?;

        let mut result = BTreeMap::new();
        for state_result in states {
            let (task_id_val, state_json) = state_result?;

            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let task_id_u32 = task_id_val as u32;
            let task_id = TaskId::new(task_id_u32);

            let state: TaskState = serde_json::from_str(&state_json)?;
            result.insert(task_id, state);
        }

        Ok(result)
    }

    /// Rebuild materialized state by replaying all events from the journal.
    ///
    /// Clears the `task_state` table and reconstructs the state for every task
    /// by replaying all events through the state machine. This ensures that
    /// the materialized state is consistent with the event journal, allowing
    /// recovery from corruption or intentional clearing.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The `task_state` table cannot be cleared
    /// - Events cannot be read from the journal
    /// - An invalid state transition is encountered (includes the offending sequence number)
    /// - The state cannot be stored
    pub fn rebuild_state(&mut self) -> Result<()> {
        use crate::state;

        // Clear the task_state table
        self.conn.execute("DELETE FROM task_state", [])?;

        // Collect all events
        let events = self.events()?;

        // Build a map of task_id -> list of events for that task
        let mut task_events: BTreeMap<TaskId, Vec<Event>> = BTreeMap::new();
        for event in events {
            if let Some(task_id) = event.task_id {
                task_events.entry(task_id).or_default().push(event);
            }
        }

        // Replay events for each task
        for (task_id, task_event_list) in task_events {
            // Start with Queued state
            let mut current_state = TaskState::Queued;

            // Apply each event in order
            for event in task_event_list {
                current_state = state::apply(&current_state, &event.kind).map_err(|e| {
                    // Convert invalid transition to Corrupt with sequence number
                    match e {
                        Error::InvalidTransition { from, event: evt } => Error::Corrupt {
                            detail: format!("Invalid transition from {from} on {evt}"),
                            seq: Some(event.seq.get()),
                        },
                        other => other,
                    }
                })?;
            }

            // Store the final state
            self.put_state(task_id, &current_state)?;
        }

        Ok(())
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

    #[test]
    fn put_state_stores_and_retrieves_task_state() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(42);
        let state = TaskState::Queued;

        journal.put_state(task_id, &state).unwrap();

        // Verify it was stored
        let stored: Option<TaskState> = journal.get_state(task_id).unwrap();
        assert_eq!(stored, Some(TaskState::Queued));

        drop(journal);
    }

    #[test]
    fn put_state_overwrites_existing_state() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(42);

        // Insert first state
        journal.put_state(task_id, &TaskState::Queued).unwrap();
        let first_count: i64 = journal
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_state WHERE task_id = ?1",
                rusqlite::params![42i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_count, 1);

        // Overwrite with second state
        journal.put_state(task_id, &TaskState::Preflight).unwrap();

        // Verify only one row exists and it has the new state
        let second_count: i64 = journal
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_state WHERE task_id = ?1",
                rusqlite::params![42i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second_count, 1);

        let stored: Option<TaskState> = journal.get_state(task_id).unwrap();
        assert_eq!(stored, Some(TaskState::Preflight));

        drop(journal);
    }

    #[test]
    fn get_state_returns_none_for_nonexistent_task() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(999);

        let stored: Option<TaskState> = journal.get_state(task_id).unwrap();
        assert_eq!(stored, None);

        drop(journal);
    }

    #[test]
    fn all_states_returns_empty_for_empty_table() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let journal = Journal::open(&journal_path).unwrap();
        let states = journal.all_states().unwrap();

        assert_eq!(states.len(), 0);

        drop(journal);
    }

    #[test]
    fn all_states_returns_tasks_in_id_order() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();

        // Insert states in non-sequential order
        journal
            .put_state(TaskId::new(5), &TaskState::Queued)
            .unwrap();
        journal
            .put_state(TaskId::new(1), &TaskState::Preflight)
            .unwrap();
        journal.put_state(TaskId::new(3), &TaskState::Done).unwrap();
        journal
            .put_state(TaskId::new(2), &TaskState::Cancelled)
            .unwrap();

        let states = journal.all_states().unwrap();

        // Verify states are returned in order by task ID
        let ids: Vec<u32> = states.keys().map(|id| id.get()).collect();
        assert_eq!(ids, vec![1, 2, 3, 5]);

        // Verify each state matches what we stored
        assert_eq!(states.get(&TaskId::new(1)), Some(&TaskState::Preflight));
        assert_eq!(states.get(&TaskId::new(2)), Some(&TaskState::Cancelled));
        assert_eq!(states.get(&TaskId::new(3)), Some(&TaskState::Done));
        assert_eq!(states.get(&TaskId::new(5)), Some(&TaskState::Queued));

        drop(journal);
    }

    #[test]
    fn put_state_with_complex_state() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(42);

        // Create a complex state with nested data
        let state = TaskState::Paused {
            reason: crate::state::PauseReason::HumanGate,
            resume_to: Box::new(TaskState::Running {
                attempt: crate::ids::AttemptId::new(2),
                phase: crate::state::Phase::Implement,
            }),
        };

        journal.put_state(task_id, &state).unwrap();

        let stored: Option<TaskState> = journal.get_state(task_id).unwrap();
        assert_eq!(stored, Some(state));

        drop(journal);
    }

    #[test]
    fn all_states_survives_reopen() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        // First session: store some states
        {
            let mut journal = Journal::open(&journal_path).unwrap();
            journal
                .put_state(TaskId::new(1), &TaskState::Queued)
                .unwrap();
            journal
                .put_state(TaskId::new(2), &TaskState::Preflight)
                .unwrap();
            drop(journal);
        }

        // Second session: verify they're still there
        {
            let journal = Journal::open(&journal_path).unwrap();
            let states = journal.all_states().unwrap();

            assert_eq!(states.len(), 2);
            assert_eq!(states.get(&TaskId::new(1)), Some(&TaskState::Queued));
            assert_eq!(states.get(&TaskId::new(2)), Some(&TaskState::Preflight));
            drop(journal);
        }
    }

    #[test]
    fn rebuild_state_from_empty_journal() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();

        // Start with no events and no state
        let states_before = journal.all_states().unwrap();
        assert_eq!(states_before.len(), 0);

        // Rebuild should succeed even with no events
        journal.rebuild_state().unwrap();

        let states_after = journal.all_states().unwrap();
        assert_eq!(states_after.len(), 0);

        drop(journal);
    }

    #[test]
    fn rebuild_state_single_task_queued() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(1);

        // Append a TaskQueued event
        let kind = EventKind::TaskQueued {
            title: "Test task".to_string(),
        };
        journal.append(Some(task_id), &kind).unwrap();

        // Store some initial state (simulating previous work)
        journal.put_state(task_id, &TaskState::Preflight).unwrap();

        // Verify the state before rebuild
        assert_eq!(
            journal.get_state(task_id).unwrap(),
            Some(TaskState::Preflight)
        );

        // Rebuild state
        journal.rebuild_state().unwrap();

        // After rebuild, state should be Queued (the result of replaying TaskQueued)
        assert_eq!(journal.get_state(task_id).unwrap(), Some(TaskState::Queued));

        drop(journal);
    }

    #[test]
    fn rebuild_state_multiple_events_same_task() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(1);

        // Append a sequence of events
        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .unwrap();
        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .unwrap();
        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .unwrap();
        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::ids::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
            )
            .unwrap();

        // Store a wrong initial state
        journal.put_state(task_id, &TaskState::Done).unwrap();

        // Verify the wrong state before rebuild
        assert_eq!(journal.get_state(task_id).unwrap(), Some(TaskState::Done));

        // Rebuild state
        journal.rebuild_state().unwrap();

        // After rebuild, state should reflect the last event
        let rebuilt_state = journal.get_state(task_id).unwrap().unwrap();
        assert!(matches!(
            rebuilt_state,
            TaskState::Running {
                attempt: crate::ids::AttemptId(1),
                phase: crate::state::Phase::Goal,
            }
        ));

        drop(journal);
    }

    #[test]
    fn rebuild_state_multiple_tasks() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task1 = TaskId::new(1);
        let task2 = TaskId::new(2);

        // Add events for task 1
        journal
            .append(
                Some(task1),
                &EventKind::TaskQueued {
                    title: "Task 1".to_string(),
                },
            )
            .unwrap();
        journal
            .append(Some(task1), &EventKind::PreflightStarted)
            .unwrap();

        // Add events for task 2
        journal
            .append(
                Some(task2),
                &EventKind::TaskQueued {
                    title: "Task 2".to_string(),
                },
            )
            .unwrap();

        // Store wrong states
        journal.put_state(task1, &TaskState::Done).unwrap();
        journal
            .put_state(
                task2,
                &TaskState::Failed {
                    class: crate::classify::FailureClass::PolicyFailure,
                    detail: "test".to_string(),
                },
            )
            .unwrap();

        // Rebuild state
        journal.rebuild_state().unwrap();

        // Verify rebuilt states
        let states = journal.all_states().unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states.get(&task1), Some(&TaskState::Preflight));
        assert_eq!(states.get(&task2), Some(&TaskState::Queued));

        drop(journal);
    }

    #[test]
    fn rebuild_state_clears_before_replay() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task1 = TaskId::new(1);
        let task2 = TaskId::new(2);

        // Add events only for task1
        journal
            .append(
                Some(task1),
                &EventKind::TaskQueued {
                    title: "Task 1".to_string(),
                },
            )
            .unwrap();

        // Store state for both task1 and task2
        journal.put_state(task1, &TaskState::Preflight).unwrap();
        journal.put_state(task2, &TaskState::Done).unwrap();

        let states_before = journal.all_states().unwrap();
        assert_eq!(states_before.len(), 2);

        // Rebuild state
        journal.rebuild_state().unwrap();

        // After rebuild, only task1 should exist (task2 has no events)
        let states_after = journal.all_states().unwrap();
        assert_eq!(states_after.len(), 1);
        assert!(states_after.contains_key(&task1));
        assert!(!states_after.contains_key(&task2));

        drop(journal);
    }

    #[test]
    fn rebuild_state_reports_sequence_on_invalid_transition() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(1);

        // Create a sequence of events that will result in an invalid transition
        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .unwrap();

        // Try to append an event that's invalid from Queued state
        // (e.g., TaskDone is not allowed from Queued)
        journal
            .append(
                Some(task_id),
                &EventKind::TaskDone {
                    commit: "def456".to_string(),
                },
            )
            .unwrap();

        // Rebuild should fail with the sequence number in the error
        let result = journal.rebuild_state();
        assert!(result.is_err());

        match result.unwrap_err() {
            Error::Corrupt {
                detail: _,
                seq: Some(seq),
            } => {
                // The error should reference seq 2 (the TaskDone event)
                assert_eq!(seq, 2);
            }
            e => panic!("Expected Corrupt error with sequence number, got: {e}"),
        }

        drop(journal);
    }

    #[test]
    fn rebuild_state_consistency_with_incremental_writes() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let task_id = TaskId::new(1);

        // First session: incrementally build state
        {
            let mut journal = Journal::open(&journal_path).unwrap();

            journal
                .append(
                    Some(task_id),
                    &EventKind::TaskQueued {
                        title: "Test task".to_string(),
                    },
                )
                .unwrap();
            journal.put_state(task_id, &TaskState::Queued).unwrap();

            journal
                .append(Some(task_id), &EventKind::PreflightStarted)
                .unwrap();
            journal.put_state(task_id, &TaskState::Preflight).unwrap();

            journal
                .append(
                    Some(task_id),
                    &EventKind::PreflightPassed {
                        base_sha: "abc123".to_string(),
                    },
                )
                .unwrap();
            journal.put_state(task_id, &TaskState::Preflight).unwrap();

            drop(journal);
        }

        // Second session: rebuild and compare
        {
            let mut journal = Journal::open(&journal_path).unwrap();

            // Get the state before rebuild
            let state_before = journal.get_state(task_id).unwrap();

            // Rebuild
            journal.rebuild_state().unwrap();

            // Get the state after rebuild
            let state_after = journal.get_state(task_id).unwrap();

            // They should be the same (both should be Preflight)
            assert_eq!(state_before, state_after);
            assert_eq!(state_after, Some(TaskState::Preflight));

            drop(journal);
        }
    }

    #[test]
    fn rebuild_state_with_complex_state_transition() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("journal.db");

        let mut journal = Journal::open(&journal_path).unwrap();
        let task_id = TaskId::new(1);

        // Build a complex sequence of events
        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .unwrap();
        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .unwrap();
        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .unwrap();
        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: crate::ids::AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
            )
            .unwrap();
        journal
            .append(
                Some(task_id),
                &EventKind::PhaseEntered {
                    attempt: crate::ids::AttemptId::new(1),
                    phase: crate::state::Phase::Implement,
                },
            )
            .unwrap();

        // Store a wrong state
        journal.put_state(task_id, &TaskState::Done).unwrap();

        // Rebuild
        journal.rebuild_state().unwrap();

        // Verify the state matches the expected transition
        let rebuilt_state = journal.get_state(task_id).unwrap().unwrap();
        assert!(matches!(
            rebuilt_state,
            TaskState::Running {
                phase: crate::state::Phase::Implement,
                ..
            }
        ));

        drop(journal);
    }

    // Property tests for journal replay invariant
    mod property_tests {
        use super::*;
        use crate::{AttemptId, FailureClass, PauseReason, Phase, Stream, state};
        use proptest::prelude::*;

        fn happy_path_sequence() -> Vec<EventKind> {
            let attempt_id = AttemptId::new(1);
            vec![
                EventKind::TaskQueued {
                    title: "Test".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: attempt_id,
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: attempt_id,
                    phase: Phase::Implement,
                },
                EventKind::AgentOutput {
                    attempt: attempt_id,
                    stream: Stream::Stdout,
                    text: "Working...".to_string(),
                },
                EventKind::VerifyPassed {
                    attempt: attempt_id,
                },
                EventKind::PublishStarted {
                    attempt: attempt_id,
                    candidate_sha: "def456".to_string(),
                },
                EventKind::PublishVerified {
                    commit: "def456".to_string(),
                    remote_sha: "def456".to_string(),
                },
                EventKind::TaskDone {
                    commit: "def456".to_string(),
                },
            ]
        }

        fn preflight_failure_sequence() -> Vec<EventKind> {
            vec![
                EventKind::TaskQueued {
                    title: "Test".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightFailed {
                    class: FailureClass::EnvironmentFailure,
                    detail: "Missing SDK".to_string(),
                },
            ]
        }

        fn remediation_sequence() -> Vec<EventKind> {
            let attempt_id = AttemptId::new(1);
            let attempt_id_2 = AttemptId::new(2);
            vec![
                EventKind::TaskQueued {
                    title: "Test".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: attempt_id,
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: attempt_id,
                    phase: Phase::Implement,
                },
                EventKind::VerifyFailed {
                    attempt: attempt_id,
                    class: FailureClass::VerificationFailure,
                    detail: "Tests failed".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: attempt_id_2,
                    protocol: "direct".to_string(),
                    pid: 1235,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: attempt_id_2,
                    phase: Phase::Implement,
                },
                EventKind::VerifyPassed {
                    attempt: attempt_id_2,
                },
                EventKind::PublishStarted {
                    attempt: attempt_id_2,
                    candidate_sha: "def456".to_string(),
                },
                EventKind::PublishVerified {
                    commit: "def456".to_string(),
                    remote_sha: "def456".to_string(),
                },
                EventKind::TaskDone {
                    commit: "def456".to_string(),
                },
            ]
        }

        fn pause_sequence() -> Vec<EventKind> {
            vec![
                EventKind::TaskQueued {
                    title: "Test".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::Paused {
                    reason: PauseReason::Input,
                },
            ]
        }

        fn cancellation_sequence() -> Vec<EventKind> {
            vec![
                EventKind::TaskQueued {
                    title: "Test".to_string(),
                },
                EventKind::TaskCancelled {
                    reason: "User cancelled".to_string(),
                },
            ]
        }

        fn tdd_sequence() -> Vec<EventKind> {
            let attempt_id = AttemptId::new(1);
            vec![
                EventKind::TaskQueued {
                    title: "Test".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: attempt_id,
                    protocol: "tdd".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: attempt_id,
                    phase: Phase::Red,
                },
                EventKind::PhaseEntered {
                    attempt: attempt_id,
                    phase: Phase::Green,
                },
                EventKind::PhaseEntered {
                    attempt: attempt_id,
                    phase: Phase::Refactor,
                },
                EventKind::VerifyPassed {
                    attempt: attempt_id,
                },
                EventKind::PublishStarted {
                    attempt: attempt_id,
                    candidate_sha: "def456".to_string(),
                },
                EventKind::PublishVerified {
                    commit: "def456".to_string(),
                    remote_sha: "def456".to_string(),
                },
                EventKind::TaskDone {
                    commit: "def456".to_string(),
                },
            ]
        }

        /// Strategy for generating valid event sequences per `TaskQueued`.
        fn valid_event_sequence_strategy() -> impl Strategy<Value = Vec<EventKind>> {
            prop_oneof![
                Just(happy_path_sequence()),
                Just(preflight_failure_sequence()),
                Just(remediation_sequence()),
                Just(pause_sequence()),
                Just(cancellation_sequence()),
                Just(tdd_sequence()),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]
            #[test]
            fn prop_journal_replay_equals_projection(events in valid_event_sequence_strategy()) {
                let temp = TempDir::new().unwrap();
                let journal_path = temp.path().join("journal.db");

                let task_id = TaskId::new(1);

                // Session 1: Append events incrementally and track final state
                let incremental_final_state = {
                    let mut journal = Journal::open(&journal_path).unwrap();

                    // Append all events and keep track of final state by applying incrementally
                    let mut current_state = TaskState::Queued;
                    for event_kind in &events {
                        journal.append(Some(task_id), event_kind).unwrap();
                        // Apply the event to track what state we should end up in
                        current_state = state::apply(&current_state, event_kind).unwrap();
                    }

                    // Store the state we computed
                    journal.put_state(task_id, &current_state).unwrap();

                    current_state
                };

                // Session 2: Rebuild state from events and compare
                let rebuilt_state = {
                    let mut journal = Journal::open(&journal_path).unwrap();

                    // Rebuild from events
                    journal.rebuild_state().unwrap();

                    // Get the rebuilt state
                    journal.get_state(task_id).unwrap().unwrap()
                };

                // The invariant: incremental application must equal replay
                prop_assert_eq!(incremental_final_state, rebuilt_state,
                    "Incremental state and rebuilt state must be equal");
            }
        }

        #[test]
        fn illegal_sequences_are_rejected() {
            let temp = TempDir::new().unwrap();
            let journal_path = temp.path().join("journal.db");

            let task_id = TaskId::new(1);

            // Test 1: Invalid transition from Queued to Verifying
            {
                let mut journal = Journal::open(&journal_path).unwrap();

                journal
                    .append(
                        Some(task_id),
                        &EventKind::TaskQueued {
                            title: "Test".to_string(),
                        },
                    )
                    .unwrap();

                // Try to append an invalid event
                journal
                    .append(
                        Some(task_id),
                        &EventKind::VerifyPassed {
                            attempt: AttemptId::new(1),
                        },
                    )
                    .unwrap();

                // Rebuild should fail with Corrupt error
                let result = journal.rebuild_state();
                assert!(
                    result.is_err(),
                    "rebuild_state should fail on invalid transition"
                );

                if let Err(Error::Corrupt { seq, .. }) = result {
                    assert!(
                        seq.is_some(),
                        "Corrupt error should include sequence number"
                    );
                }

                drop(journal);
            }

            // Clean up for next test
            std::fs::remove_file(&journal_path).ok();

            // Test 2: Invalid transition from Preflight to PublishVerified
            {
                let mut journal = Journal::open(&journal_path).unwrap();

                journal
                    .append(
                        Some(task_id),
                        &EventKind::TaskQueued {
                            title: "Test".to_string(),
                        },
                    )
                    .unwrap();

                journal
                    .append(Some(task_id), &EventKind::PreflightStarted)
                    .unwrap();

                // Try to skip to publishing
                journal
                    .append(
                        Some(task_id),
                        &EventKind::PublishVerified {
                            commit: "abc123".to_string(),
                            remote_sha: "abc123".to_string(),
                        },
                    )
                    .unwrap();

                // Rebuild should fail
                let result = journal.rebuild_state();
                assert!(result.is_err(), "Invalid sequence should fail rebuild");

                drop(journal);
            }
        }
    }
}

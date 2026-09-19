//! Event journal for storing and retrieving task events.

use crate::{Error, EventKind, EventSeq, Project, Result, TaskId};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

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
        let ts = time::OffsetDateTime::now_utc();
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

        let before = time::OffsetDateTime::now_utc();
        let mut journal = Journal::open(&journal_path).unwrap();
        let kind = EventKind::PreflightStarted;

        journal.append(None, &kind).unwrap();
        let after = time::OffsetDateTime::now_utc();

        // Verify timestamp was stored
        let ts_str: String = journal
            .conn
            .query_row("SELECT ts FROM events WHERE seq = 1", [], |row| row.get(0))
            .unwrap();

        // Parse the timestamp
        let ts =
            time::OffsetDateTime::parse(&ts_str, &time::format_description::well_known::Rfc3339)
                .unwrap();

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
}

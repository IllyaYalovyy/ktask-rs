//! `Journal`: the event journal's SQLite-backed database.
//!
//! One journal database exists per project, at `<state_dir>/journal.db`.
//! Opening it is idempotent: the schema is applied with `CREATE TABLE IF NOT
//! EXISTS`, so an existing, up-to-date journal is left untouched, and a
//! journal missing tables (for instance, one that so far only holds the
//! `meta` table [`crate::Project::register`] writes) has the rest filled in.
//! A `schema_version` row in `meta` records how far the schema has been
//! carried; opening a journal stamped with a version newer than this build
//! understands is a typed [`Error::Corrupt`], not a best-effort read of a
//! layout it does not recognize.

use crate::{Error, EventKind, EventSeq, Project, Result, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The schema version this build creates and understands.
///
/// Stored under the `schema_version` key in the `meta` table of every
/// journal this build opens.
const SCHEMA_VERSION: i64 = 1;

/// The schema shared by every journal database: append-only events, the
/// queue's tasks, each task's current state projection, and a small
/// key/value table for metadata including [`SCHEMA_VERSION`].
///
/// Every statement is `IF NOT EXISTS`, so applying this to an existing
/// database only fills in whatever tables or indexes are still missing.
/// Journal mode is set to WAL and `synchronous` to `FULL`: losing the last
/// committed event is exactly the failure this design exists to prevent.
const SCHEMA_SQL: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS events (
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts       TEXT    NOT NULL,
  task_id  INTEGER,
  kind     TEXT    NOT NULL,
  payload  TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id, seq);
CREATE TABLE IF NOT EXISTS tasks (
  id         INTEGER PRIMARY KEY,
  title      TEXT    NOT NULL,
  outcome    TEXT    NOT NULL,
  done_when  TEXT    NOT NULL,
  verify     TEXT    NOT NULL,
  refs       TEXT    NOT NULL,
  protocol   TEXT,
  body       TEXT    NOT NULL,
  added_at   TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS task_state (
  task_id    INTEGER PRIMARY KEY,
  state_json TEXT    NOT NULL,
  updated_at TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

/// A project's event journal: the append-only `events` table, the `tasks`
/// and `task_state` projections, and `meta`, all in one SQLite file.
#[derive(Debug)]
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Opens (creating or upgrading as needed) the journal database at
    /// `path`.
    ///
    /// Applies the journal schema, which is idempotent: opening an existing,
    /// up-to-date journal changes nothing beyond confirming its schema. A
    /// journal with no recorded `schema_version` (a fresh database, or one
    /// created before this table existed) is stamped with this build's own
    /// schema version.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] when the database cannot be opened or the
    /// schema cannot be applied, and [`Error::Corrupt`] when the journal's
    /// recorded `schema_version` is not a valid integer or is newer than
    /// this build understands.
    pub fn open(path: &Path) -> Result<Journal> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        let journal = Journal { conn };
        journal.ensure_schema_version()?;
        Ok(journal)
    }

    /// Opens `project`'s journal database, at [`journal_path`] under its
    /// state directory.
    ///
    /// # Errors
    ///
    /// See [`Journal::open`].
    pub fn open_for(project: &Project) -> Result<Journal> {
        Journal::open(&journal_path(&project.state_dir))
    }

    /// Appends `kind` to the journal, returning the sequence number assigned
    /// to it.
    ///
    /// The timestamp is stamped as the current UTC instant inside this
    /// function, and `kind` is serialized to JSON before anything is
    /// written. The insert runs inside a transaction, so a failure partway
    /// through — serializing the payload, or the insert itself — leaves the
    /// journal completely unchanged: a sequence number is only ever handed
    /// out for an event that is durably recorded. Sequence numbers are
    /// strictly increasing, both within a session and across the journal
    /// being closed and reopened, because they come from SQLite's
    /// `AUTOINCREMENT`, which never reuses a value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Serde`] if `kind` cannot be serialized to JSON,
    /// [`Error::Time`] if the current instant cannot be formatted, and
    /// [`Error::Database`] if the insert fails.
    pub fn append(&mut self, task_id: Option<TaskId>, kind: &EventKind) -> Result<EventSeq> {
        let payload = serde_json::to_string(kind)?;
        let ts = OffsetDateTime::now_utc().format(&Rfc3339)?;

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO events (ts, task_id, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![
                ts,
                task_id.map(|id| i64::from(id.get())),
                kind.discriminant(),
                payload,
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;

        let seq = u64::try_from(seq).map_err(|_| Error::Corrupt {
            detail: format!("journal produced a negative event sequence number: {seq}"),
        })?;
        Ok(EventSeq::new(seq))
    }

    /// Reads this journal's recorded `schema_version`, stamping one for a
    /// freshly created database or rejecting one newer than
    /// [`SCHEMA_VERSION`].
    fn ensure_schema_version(&self) -> Result<()> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        let Some(stored) = stored else {
            self.conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION.to_string()],
            )?;
            return Ok(());
        };

        let version: i64 = stored.parse().map_err(|_| Error::Corrupt {
            detail: format!("journal meta.schema_version is not an integer: {stored:?}"),
        })?;

        if version > SCHEMA_VERSION {
            return Err(Error::Corrupt {
                detail: format!(
                    "journal schema version {version} is newer than this build supports \
                     (build supports up to {SCHEMA_VERSION})"
                ),
            });
        }

        Ok(())
    }
}

/// Returns the path to a project's journal database: `<state_dir>/journal.db`.
#[must_use]
pub fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join("journal.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_all_tables_and_the_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        let journal = Journal::open(&path).expect("open");

        let mut names: Vec<String> = journal
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'index') ORDER BY name",
            )
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        names.sort();

        for expected in ["events", "idx_events_task", "meta", "task_state", "tasks"] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }

    #[test]
    fn open_sets_wal_journal_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        let journal = Journal::open(&path).expect("open");

        let mode: String = journal
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn open_sets_synchronous_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        let journal = Journal::open(&path).expect("open");

        // SQLite reports `synchronous` back as its integer level: OFF=0,
        // NORMAL=1, FULL=2, EXTRA=3.
        let level: i64 = journal
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("read synchronous");
        assert_eq!(level, 2);
    }

    #[test]
    fn open_stamps_the_current_schema_version_on_a_fresh_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        let journal = Journal::open(&path).expect("open");

        let version: String = journal
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_version row");
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn opening_twice_preserves_data_and_does_not_duplicate_schema_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        {
            let journal = Journal::open(&path).expect("first open");
            journal
                .conn
                .execute(
                    "INSERT INTO events (ts, task_id, kind, payload) \
                     VALUES ('2024-01-01T00:00:00Z', NULL, 'TaskQueued', '{}')",
                    [],
                )
                .expect("insert event");
        }

        let journal = Journal::open(&path).expect("second open");

        let event_count: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count events");
        assert_eq!(event_count, 1, "second open must not touch existing rows");

        let version_count: i64 = journal
            .conn
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("count schema_version rows");
        assert_eq!(version_count, 1, "second open must not duplicate the row");
    }

    #[test]
    fn opening_a_journal_missing_tables_fills_them_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        {
            // Mirrors what `Project::register` writes today: only `meta`.
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .expect("create meta only");
        }

        let journal = Journal::open(&path).expect("open must upgrade the schema");

        let count: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("events table must now exist");
        assert_eq!(count, 0);
    }

    #[test]
    fn opening_a_future_schema_version_is_a_clear_corrupt_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(SCHEMA_SQL).expect("create schema");
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '999')",
                [],
            )
            .expect("stamp future version");
        }

        let err = Journal::open(&path).expect_err("must reject a future schema version");
        assert!(
            matches!(&err, Error::Corrupt { detail } if detail.contains("999")),
            "expected a Corrupt error naming the future version, got {err:?}"
        );
    }

    #[test]
    fn opening_a_non_numeric_schema_version_is_a_clear_corrupt_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(SCHEMA_SQL).expect("create schema");
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', 'not-a-number')",
                [],
            )
            .expect("stamp bogus version");
        }

        let err = Journal::open(&path).expect_err("must reject a non-numeric schema version");
        assert!(matches!(&err, Error::Corrupt { .. }));
    }

    #[test]
    fn journal_path_appends_journal_db_to_the_state_dir() {
        let state_dir = PathBuf::from("/tmp/example/state");
        assert_eq!(journal_path(&state_dir), state_dir.join("journal.db"));
    }

    #[test]
    fn open_for_opens_the_projects_journal_database() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let project = Project {
            root: PathBuf::from("/repo"),
            id: "abc123".to_string(),
            state_dir: state_dir.path().to_path_buf(),
        };

        let _journal = Journal::open_for(&project).expect("open_for");

        assert!(journal_path(&project.state_dir).is_file());
    }

    #[test]
    fn append_returns_strictly_increasing_sequence_numbers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let first = journal.append(None, &EventKind::Resumed).expect("append 1");
        let second = journal.append(None, &EventKind::Resumed).expect("append 2");
        let third = journal.append(None, &EventKind::Resumed).expect("append 3");

        assert!(first.get() < second.get(), "{first} !< {second}");
        assert!(second.get() < third.get(), "{second} !< {third}");
    }

    #[test]
    fn append_sequence_numbers_stay_monotonic_across_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        let last_before_close = {
            let mut journal = Journal::open(&path).expect("first open");
            journal.append(None, &EventKind::Resumed).expect("append 1");
            journal.append(None, &EventKind::Resumed).expect("append 2")
        };

        let mut journal = Journal::open(&path).expect("reopen");
        let after_reopen = journal
            .append(None, &EventKind::Resumed)
            .expect("append after reopen");

        assert!(
            after_reopen.get() > last_before_close.get(),
            "{after_reopen} !> {last_before_close}"
        );
    }

    #[test]
    fn append_writes_the_row_with_the_discriminant_and_a_json_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let event = EventKind::TaskQueued {
            title: "Add widget".to_string(),
        };
        let seq = journal
            .append(Some(TaskId::new(7)), &event)
            .expect("append");

        let (kind, payload, task_id): (String, String, Option<i64>) = journal
            .conn
            .query_row(
                "SELECT kind, payload, task_id FROM events WHERE seq = ?1",
                [i64::try_from(seq.get()).expect("seq fits in i64")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read back the inserted row");

        assert_eq!(kind, event.discriminant());
        assert_eq!(task_id, Some(7));
        let decoded: EventKind = serde_json::from_str(&payload).expect("payload is valid JSON");
        assert_eq!(decoded, event);
    }

    #[test]
    fn append_with_no_task_stores_a_null_task_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let seq = journal.append(None, &EventKind::Resumed).expect("append");

        let task_id: Option<i64> = journal
            .conn
            .query_row(
                "SELECT task_id FROM events WHERE seq = ?1",
                [i64::try_from(seq.get()).expect("seq fits in i64")],
                |row| row.get(0),
            )
            .expect("read back the inserted row");

        assert_eq!(task_id, None);
    }

    #[test]
    fn append_leaves_nothing_written_when_the_insert_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        // Break the schema so the transaction's INSERT fails after the
        // payload has already been serialized, proving the transaction
        // wrapper rolls the row back rather than leaving a partial write.
        journal
            .conn
            .execute_batch("ALTER TABLE events RENAME COLUMN kind TO kind_renamed")
            .expect("break the schema");

        let err = journal
            .append(None, &EventKind::Resumed)
            .expect_err("insert must fail once the schema no longer matches");
        assert!(matches!(err, Error::Database(_)));

        let count: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count events");
        assert_eq!(count, 0, "a failed insert must not leave a partial row");
    }
}

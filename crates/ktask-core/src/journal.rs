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

use crate::task::status_from_body;
use crate::{Error, Event, EventKind, EventSeq, Project, Result, Task, TaskId, TaskState, apply};
use rusqlite::{Connection, OptionalExtension, Params, params};
use std::collections::BTreeMap;
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
///
/// `events` is append-only in fact, not only by convention (VISION.md
/// section 3, invariant 3): triggers reject any `UPDATE` or `DELETE`
/// against it, turning a mutation attempt into a SQLite error instead of a
/// silently rewritten history.
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
CREATE TRIGGER IF NOT EXISTS trg_events_no_update
BEFORE UPDATE ON events
BEGIN
  SELECT RAISE(ABORT, 'events is append-only: UPDATE is not permitted');
END;
CREATE TRIGGER IF NOT EXISTS trg_events_no_delete
BEFORE DELETE ON events
BEGIN
  SELECT RAISE(ABORT, 'events is append-only: DELETE is not permitted');
END;
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

    /// Imports `tasks` into the queue, in document order.
    ///
    /// This is a one-time import, not a merge: a plan file is an input
    /// format only, and once its blocks are in the database the file has no
    /// further hold over the run. A task's status is not written here — it
    /// is derived from the journal, so status has exactly one home;
    /// [`Journal::tasks`] recovers the same value back from the stored body
    /// instead.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Policy`] naming the existing task count if the queue
    /// is not empty: there is no merge and no in-place edit. Returns
    /// [`Error::Time`] if the current instant cannot be formatted, and
    /// [`Error::Database`] if the insert fails.
    pub fn put_tasks(&mut self, tasks: &[Task]) -> Result<()> {
        let existing: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))?;
        if existing > 0 {
            return Err(Error::Policy {
                detail: format!(
                    "queue already has {existing} task(s); import does not merge or edit in place"
                ),
                paths: Vec::new(),
            });
        }

        let added_at = OffsetDateTime::now_utc().format(&Rfc3339)?;

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tasks (id, title, outcome, done_when, verify, refs, protocol, body, added_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for task in tasks {
                stmt.execute(params![
                    i64::from(task.id.get()),
                    task.title(),
                    task.outcome,
                    task.done_when,
                    task.verify,
                    task.refs,
                    Option::<String>::None,
                    task.body,
                    added_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns every task in the queue, ordered by id.
    ///
    /// A task's status is not a stored column: it is recomputed from the
    /// stored `body` with the same rule [`crate::task::parse_plan`] applies
    /// while building a task, so a task read back here is identical to the
    /// one that was written by [`Journal::put_tasks`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, and
    /// [`Error::Corrupt`] if a stored `id` does not fit a [`TaskId`].
    pub fn tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, outcome, done_when, verify, refs, body FROM tasks ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let outcome: String = row.get(1)?;
            let done_when: String = row.get(2)?;
            let verify: String = row.get(3)?;
            let refs: String = row.get(4)?;
            let body: String = row.get(5)?;
            Ok((id, outcome, done_when, verify, refs, body))
        })?;

        let mut tasks = Vec::new();
        for row in rows {
            let (id, outcome, done_when, verify, refs, body) = row?;
            let id = u32::try_from(id).map_err(|_| Error::Corrupt {
                detail: format!("tasks table has an invalid id: {id}"),
            })?;
            tasks.push(Task {
                id: TaskId::new(id),
                status: status_from_body(&body),
                body,
                outcome,
                done_when,
                verify,
                refs,
            });
        }
        Ok(tasks)
    }

    /// Writes `state` as `task`'s current materialized state, overwriting
    /// whatever was previously stored for it rather than duplicating a row.
    ///
    /// `task_state` is a projection (`docs/DESIGN.md` Database schema): it
    /// may be dropped and rebuilt by replaying `events`, so this only ever
    /// needs to hold one row per task.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Serde`] if `state` cannot be serialized to JSON,
    /// [`Error::Time`] if the current instant cannot be formatted, and
    /// [`Error::Database`] if the write fails.
    pub fn put_state(&mut self, task: TaskId, state: &TaskState) -> Result<()> {
        let state_json = serde_json::to_string(state)?;
        let updated_at = OffsetDateTime::now_utc().format(&Rfc3339)?;

        self.conn.execute(
            "INSERT INTO task_state (task_id, state_json, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(task_id) DO UPDATE SET \
               state_json = excluded.state_json, updated_at = excluded.updated_at",
            params![i64::from(task.get()), state_json, updated_at],
        )?;
        Ok(())
    }

    /// Returns `task`'s materialized state, or `None` if [`Journal::put_state`]
    /// has never been called for it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, and [`Error::Serde`]
    /// if the stored `state_json` is not valid JSON for [`TaskState`].
    pub fn get_state(&self, task: TaskId) -> Result<Option<TaskState>> {
        let state_json: Option<String> = self
            .conn
            .query_row(
                "SELECT state_json FROM task_state WHERE task_id = ?1",
                params![i64::from(task.get())],
                |row| row.get(0),
            )
            .optional()?;

        state_json
            .map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }

    /// Returns every task's materialized state, keyed by [`TaskId`] in id
    /// order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, [`Error::Serde`] if a
    /// stored `state_json` is not valid JSON for [`TaskState`], and
    /// [`Error::Corrupt`] if a stored `task_id` does not fit a [`TaskId`].
    pub fn all_states(&self) -> Result<BTreeMap<TaskId, TaskState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_id, state_json FROM task_state ORDER BY task_id ASC")?;
        let rows = stmt.query_map([], |row| {
            let task_id: i64 = row.get(0)?;
            let state_json: String = row.get(1)?;
            Ok((task_id, state_json))
        })?;

        let mut states = BTreeMap::new();
        for row in rows {
            let (task_id, state_json) = row?;
            let task_id = u32::try_from(task_id).map_err(|_| Error::Corrupt {
                detail: format!("task_state table has an invalid task_id: {task_id}"),
            })?;
            let state: TaskState = serde_json::from_str(&state_json)?;
            states.insert(TaskId::new(task_id), state);
        }
        Ok(states)
    }

    /// Drops and rebuilds `task_state` by replaying every event in the
    /// journal through [`apply`].
    ///
    /// Each task's events (its `task_id` is not `NULL`) are replayed in
    /// `seq` order starting from [`TaskState::Queued`], mirroring exactly
    /// what incrementally calling [`Journal::put_state`] after every event
    /// would have produced (`docs/DESIGN.md`: `task_state` is a projection
    /// and may be dropped and rebuilt by replay). Queue-level events, whose
    /// `task_id` is `NULL`, do not belong to any task's state machine and
    /// are skipped.
    ///
    /// The whole replay runs in memory before `task_state` is touched: if
    /// any event fails to apply, the existing projection is left exactly as
    /// it was, rather than partially rewritten.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if reading or writing the database
    /// fails, [`Error::Serde`] if a stored payload is not valid JSON, and
    /// [`Error::Corrupt`] if a stored `seq`, `ts` or `task_id` is not a
    /// value this build can represent, or if replay hits an event that does
    /// not apply to the state it followed — naming the offending sequence
    /// number rather than stopping silently.
    pub fn rebuild_state(&mut self) -> Result<()> {
        let events = self.events()?;

        let mut states: BTreeMap<TaskId, TaskState> = BTreeMap::new();
        for event in &events {
            let Some(task_id) = event.task_id else {
                continue;
            };
            let current = states.entry(task_id).or_insert(TaskState::Queued);
            *current = apply(current, &event.kind).map_err(|source| Error::Corrupt {
                detail: format!(
                    "journal replay hit an invalid transition at event seq {}: {source}",
                    event.seq
                ),
            })?;
        }

        self.conn.execute("DELETE FROM task_state", [])?;
        for (task_id, state) in &states {
            self.put_state(*task_id, state)?;
        }
        Ok(())
    }

    /// Returns every event in the journal, ordered by `seq` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, [`Error::Serde`] if a
    /// stored payload is not valid JSON for [`EventKind`], and
    /// [`Error::Corrupt`] if a stored `seq`, `ts` or `task_id` is not a
    /// value this build can represent.
    pub fn events(&self) -> Result<Vec<Event>> {
        self.query_events(
            "SELECT seq, ts, task_id, payload FROM events ORDER BY seq ASC",
            [],
        )
    }

    /// Returns every event recorded against `task`, ordered by `seq`
    /// ascending.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, [`Error::Serde`] if a
    /// stored payload is not valid JSON for [`EventKind`], and
    /// [`Error::Corrupt`] if a stored `seq`, `ts` or `task_id` is not a
    /// value this build can represent.
    pub fn events_for(&self, task: TaskId) -> Result<Vec<Event>> {
        self.query_events(
            "SELECT seq, ts, task_id, payload FROM events WHERE task_id = ?1 ORDER BY seq ASC",
            params![i64::from(task.get())],
        )
    }

    /// Returns every event recorded after `seq` (exclusive), ordered by
    /// `seq` ascending: what a caller that last saw `seq` has missed since.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, [`Error::Serde`] if a
    /// stored payload is not valid JSON for [`EventKind`], and
    /// [`Error::Corrupt`] if a stored `seq`, `ts` or `task_id` is not a
    /// value this build can represent.
    pub fn events_since(&self, seq: EventSeq) -> Result<Vec<Event>> {
        let seq = i64::try_from(seq.get()).map_err(|_| Error::Corrupt {
            detail: format!("event sequence number does not fit in i64: {}", seq.get()),
        })?;
        self.query_events(
            "SELECT seq, ts, task_id, payload FROM events WHERE seq > ?1 ORDER BY seq ASC",
            params![seq],
        )
    }

    /// Runs `sql` (which must select `seq, ts, task_id, payload` in that
    /// order) with `params`, decoding each row into an [`Event`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails, [`Error::Serde`] if a
    /// stored payload is not valid JSON for [`EventKind`], and
    /// [`Error::Corrupt`] if a stored `seq`, `ts` or `task_id` is not a
    /// value this build can represent.
    fn query_events(&self, sql: &str, params: impl Params) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params)?;

        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(decode_event(row)?);
        }

        Ok(events)
    }

    /// Streams every event recorded after `from` (exclusive), ordered by
    /// `seq` ascending, invoking `f` once per event as it is read from the
    /// prepared statement rather than collecting the whole result into a
    /// `Vec` first.
    ///
    /// This is [`Journal::events_since`]'s streaming counterpart: reading a
    /// journal with a very large event count should cost memory
    /// proportional to one row, not to the whole journal.
    ///
    /// # Errors
    ///
    /// Returns whatever `f` returns on the first event it errors on,
    /// short-circuiting the scan. Otherwise returns [`Error::Database`] if
    /// the query fails, [`Error::Serde`] if a stored payload is not valid
    /// JSON for [`EventKind`], and [`Error::Corrupt`] if a stored `seq`,
    /// `ts` or `task_id` is not a value this build can represent.
    pub fn for_each_event(
        &self,
        from: EventSeq,
        f: &mut dyn FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let from = i64::try_from(from.get()).map_err(|_| Error::Corrupt {
            detail: format!("event sequence number does not fit in i64: {}", from.get()),
        })?;

        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, task_id, payload FROM events WHERE seq > ?1 ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query(params![from])?;
        while let Some(row) = rows.next()? {
            f(decode_event(row)?)?;
        }
        Ok(())
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

/// Decodes one `events` row (`seq, ts, task_id, payload`, in that order)
/// into an [`Event`].
///
/// # Errors
///
/// Returns [`Error::Database`] if a column cannot be read, [`Error::Serde`]
/// if `payload` is not valid JSON for [`EventKind`], and [`Error::Corrupt`]
/// if `seq`, `ts` or `task_id` is not a value this build can represent.
fn decode_event(row: &rusqlite::Row<'_>) -> Result<Event> {
    let seq: i64 = row.get(0)?;
    let ts: String = row.get(1)?;
    let task_id: Option<i64> = row.get(2)?;
    let payload: String = row.get(3)?;

    let seq = EventSeq::new(u64::try_from(seq).map_err(|_| Error::Corrupt {
        detail: format!("journal has a negative event sequence number: {seq}"),
    })?);

    let ts = OffsetDateTime::parse(&ts, &Rfc3339).map_err(|source| Error::Corrupt {
        detail: format!("journal event {seq} has an invalid timestamp {ts:?}: {source}"),
    })?;

    let task_id = task_id
        .map(|id| {
            u32::try_from(id).map_err(|_| Error::Corrupt {
                detail: format!("journal event {seq} has an invalid task_id: {id}"),
            })
        })
        .transpose()?
        .map(TaskId::new);

    let kind: EventKind = serde_json::from_str(&payload)?;

    Ok(Event {
        seq,
        ts,
        task_id,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, FailureClass, Phase};

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
    fn open_creates_the_append_only_triggers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        let journal = Journal::open(&path).expect("open");

        let mut names: Vec<String> = journal
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' ORDER BY name")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        names.sort();

        for expected in ["trg_events_no_delete", "trg_events_no_update"] {
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
    fn updating_an_event_row_is_rejected_and_leaves_it_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let seq = journal.append(None, &EventKind::Resumed).expect("append");

        let err = journal
            .conn
            .execute(
                "UPDATE events SET kind = 'TaskQueued' WHERE seq = ?1",
                [i64::try_from(seq.get()).expect("seq fits in i64")],
            )
            .expect_err("UPDATE against events must be rejected");
        assert!(
            err.to_string().contains("append-only"),
            "expected an append-only error, got {err}"
        );

        let kind: String = journal
            .conn
            .query_row(
                "SELECT kind FROM events WHERE seq = ?1",
                [i64::try_from(seq.get()).expect("seq fits in i64")],
                |row| row.get(0),
            )
            .expect("read back the row");
        assert_eq!(
            kind,
            EventKind::Resumed.discriminant(),
            "row must be unchanged"
        );
    }

    #[test]
    fn deleting_an_event_row_is_rejected_and_leaves_it_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let seq = journal.append(None, &EventKind::Resumed).expect("append");

        let err = journal
            .conn
            .execute(
                "DELETE FROM events WHERE seq = ?1",
                [i64::try_from(seq.get()).expect("seq fits in i64")],
            )
            .expect_err("DELETE against events must be rejected");
        assert!(
            err.to_string().contains("append-only"),
            "expected an append-only error, got {err}"
        );

        let count: i64 = journal
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE seq = ?1",
                [i64::try_from(seq.get()).expect("seq fits in i64")],
                |row| row.get(0),
            )
            .expect("count matching rows");
        assert_eq!(count, 1, "row must still be present");
    }

    #[test]
    fn events_on_an_empty_journal_returns_an_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open");

        assert_eq!(journal.events().expect("events"), Vec::new());
    }

    #[test]
    fn events_returns_a_single_appended_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let kind = EventKind::TaskQueued {
            title: "Add widget".to_string(),
        };
        let seq = journal.append(Some(TaskId::new(3)), &kind).expect("append");

        let events = journal.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, seq);
        assert_eq!(events[0].task_id, Some(TaskId::new(3)));
        assert_eq!(events[0].kind, kind);
    }

    #[test]
    fn events_orders_by_sequence_not_by_timestamp_or_insertion_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open");

        // Insert rows directly with explicit `seq` values, out of both
        // insertion order and timestamp order: the row inserted first has
        // the highest `seq` and the earliest `ts`, and the row inserted
        // last has the lowest `seq` and the latest `ts`. `append` itself
        // can never produce such a journal, but a corrupted clock or a
        // manual repair could; `events()` must still recover `seq` order,
        // not the order rows were inserted in or the order their
        // timestamps suggest.
        for (seq, ts) in [
            (100_i64, "2020-01-01T00:00:00Z"),
            (1_i64, "2020-01-03T00:00:00Z"),
            (50_i64, "2020-01-02T00:00:00Z"),
        ] {
            journal
                .conn
                .execute(
                    "INSERT INTO events (seq, ts, task_id, kind, payload) \
                     VALUES (?1, ?2, NULL, 'Resumed', '{\"kind\":\"Resumed\"}')",
                    params![seq, ts],
                )
                .expect("insert out-of-order row");
        }

        let events = journal.events().expect("events");
        let seqs: Vec<u64> = events.iter().map(|e| e.seq.get()).collect();
        assert_eq!(seqs, vec![1, 50, 100]);
    }

    #[test]
    fn events_for_returns_only_that_tasks_events_in_seq_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let task_a = TaskId::new(1);
        let task_b = TaskId::new(2);

        let a1 = journal
            .append(Some(task_a), &EventKind::PreflightStarted)
            .expect("append a1");
        let _b1 = journal
            .append(Some(task_b), &EventKind::PreflightStarted)
            .expect("append b1");
        let a2 = journal
            .append(
                Some(task_a),
                &EventKind::TaskFailed {
                    class: FailureClass::VerificationFailure,
                    detail: "boom".to_string(),
                },
            )
            .expect("append a2");

        let events = journal.events_for(task_a).expect("events_for");
        let seqs: Vec<EventSeq> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![a1, a2]);
        assert!(events.iter().all(|e| e.task_id == Some(task_a)));
    }

    #[test]
    fn events_for_an_unknown_task_returns_an_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        journal
            .append(Some(TaskId::new(1)), &EventKind::PreflightStarted)
            .expect("append");

        assert_eq!(
            journal.events_for(TaskId::new(99)).expect("events_for"),
            Vec::new()
        );
    }

    #[test]
    fn events_since_excludes_seq_and_everything_before_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let first = journal.append(None, &EventKind::Resumed).expect("append 1");
        let second = journal
            .append(None, &EventKind::PreflightStarted)
            .expect("append 2");
        let third = journal
            .append(
                None,
                &EventKind::TaskCancelled {
                    reason: "later".to_string(),
                },
            )
            .expect("append 3");

        let events = journal.events_since(first).expect("events_since");
        let seqs: Vec<EventSeq> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![second, third]);
    }

    #[test]
    fn events_since_the_last_seq_returns_an_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let only = journal.append(None, &EventKind::Resumed).expect("append");

        assert_eq!(
            journal.events_since(only).expect("events_since"),
            Vec::new()
        );
    }

    #[test]
    fn events_since_zero_returns_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let first = journal.append(None, &EventKind::Resumed).expect("append 1");
        let second = journal
            .append(None, &EventKind::PreflightStarted)
            .expect("append 2");

        let events = journal
            .events_since(EventSeq::new(0))
            .expect("events_since");
        let seqs: Vec<EventSeq> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![first, second]);
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

    #[test]
    fn tasks_on_an_empty_queue_returns_an_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open");

        assert_eq!(journal.tasks().expect("tasks"), Vec::new());
    }

    #[test]
    fn put_tasks_then_tasks_round_trips_a_parsed_plan_unchanged_and_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let plan = "\
## First task

**Outcome:** the first thing happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none

## Second task

**Gate:** a human must approve before this proceeds.

**Outcome:** the second thing happens.

**Done-when:** it happened too.

**Verify:** `false`

**Refs:** VISION.md
";
        let parsed = crate::parse_plan(plan).expect("parse_plan");
        assert_eq!(parsed.len(), 2, "sanity: the plan has two tasks");

        journal.put_tasks(&parsed).expect("put_tasks");

        let read_back = journal.tasks().expect("tasks");
        assert_eq!(read_back, parsed);
    }

    #[test]
    fn get_state_for_a_task_with_no_stored_state_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open");

        assert_eq!(journal.get_state(TaskId::new(1)).expect("get_state"), None);
    }

    #[test]
    fn put_state_then_get_state_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        journal
            .put_state(TaskId::new(1), &TaskState::Preflight)
            .expect("put_state");

        assert_eq!(
            journal.get_state(TaskId::new(1)).expect("get_state"),
            Some(TaskState::Preflight)
        );
    }

    #[test]
    fn put_state_twice_overwrites_rather_than_duplicating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        journal
            .put_state(TaskId::new(1), &TaskState::Queued)
            .expect("first put_state");
        journal
            .put_state(TaskId::new(1), &TaskState::Preflight)
            .expect("second put_state");

        let count: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM task_state", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(count, 1, "overwriting must not duplicate the row");

        assert_eq!(
            journal.get_state(TaskId::new(1)).expect("get_state"),
            Some(TaskState::Preflight),
            "the last write must win"
        );
    }

    #[test]
    fn all_states_on_an_empty_table_returns_an_empty_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).expect("open");

        assert_eq!(journal.all_states().expect("all_states"), BTreeMap::new());
    }

    #[test]
    fn all_states_returns_every_tasks_state_in_id_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        // Written out of id order, to prove `all_states` sorts rather than
        // returning insertion order.
        journal
            .put_state(TaskId::new(3), &TaskState::Done)
            .expect("put_state 3");
        journal
            .put_state(TaskId::new(1), &TaskState::Queued)
            .expect("put_state 1");
        journal
            .put_state(TaskId::new(2), &TaskState::Preflight)
            .expect("put_state 2");

        let states = journal.all_states().expect("all_states");
        let ids: Vec<TaskId> = states.keys().copied().collect();
        assert_eq!(ids, vec![TaskId::new(1), TaskId::new(2), TaskId::new(3)]);
        assert_eq!(states[&TaskId::new(1)], TaskState::Queued);
        assert_eq!(states[&TaskId::new(2)], TaskState::Preflight);
        assert_eq!(states[&TaskId::new(3)], TaskState::Done);
    }

    #[test]
    fn rebuild_state_matches_incremental_writes_for_a_multi_task_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let task_a = TaskId::new(1);
        let task_b = TaskId::new(2);

        // Task A: queued all the way through to Done, writing state
        // incrementally the way a runner would.
        let mut state_a = TaskState::Queued;
        for kind in [
            EventKind::TaskQueued {
                title: "A".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: 123,
                base_sha: "base".to_string(),
            },
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Verify,
            },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
            EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "cand".to_string(),
            },
            EventKind::PublishVerified {
                commit: "cand".to_string(),
                remote_sha: "cand".to_string(),
            },
            EventKind::TaskDone {
                commit: "cand".to_string(),
            },
        ] {
            journal
                .append(Some(task_a), &kind)
                .expect("append task a event");
            state_a = apply(&state_a, &kind).expect("apply task a event");
            journal
                .put_state(task_a, &state_a)
                .expect("put_state task a");
        }

        // Task B: queued, then fails preflight.
        let mut state_b = TaskState::Queued;
        for kind in [
            EventKind::TaskQueued {
                title: "B".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "disk full".to_string(),
            },
        ] {
            journal
                .append(Some(task_b), &kind)
                .expect("append task b event");
            state_b = apply(&state_b, &kind).expect("apply task b event");
            journal
                .put_state(task_b, &state_b)
                .expect("put_state task b");
        }

        let expected = journal.all_states().expect("incremental all_states");
        assert_eq!(expected[&task_a], TaskState::Done, "sanity: task a is Done");
        assert!(
            matches!(expected[&task_b], TaskState::Failed { .. }),
            "sanity: task b is Failed"
        );

        journal.rebuild_state().expect("rebuild_state");

        assert_eq!(
            journal.all_states().expect("rebuilt all_states"),
            expected,
            "rebuilding from events must reproduce what incremental put_state calls produced"
        );
    }

    #[test]
    fn rebuild_state_clears_stale_state_for_a_task_no_longer_backed_by_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        // No events at all: this row has nothing in the journal to justify it.
        journal
            .put_state(
                TaskId::new(99),
                &TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
            )
            .expect("put_state stale");

        journal.rebuild_state().expect("rebuild_state");

        assert_eq!(
            journal.get_state(TaskId::new(99)).expect("get_state"),
            None,
            "a projection row with no backing events must not survive a rebuild"
        );
    }

    #[test]
    fn rebuild_state_ignores_queue_level_events_with_no_task_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        journal
            .append(None, &EventKind::Resumed)
            .expect("append queue-level event");

        journal
            .rebuild_state()
            .expect("rebuild_state must not choke on a task-less event");

        assert_eq!(journal.all_states().expect("all_states"), BTreeMap::new());
    }

    #[test]
    fn rebuild_state_on_an_invalid_transition_names_the_offending_sequence_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let task = TaskId::new(1);
        journal
            .append(
                Some(task),
                &EventKind::TaskQueued {
                    title: "A".to_string(),
                },
            )
            .expect("append 1");
        // A Queued task has never had an AttemptStarted, so VerifyPassed does
        // not apply here: this is the offending event.
        let bad_seq = journal
            .append(
                Some(task),
                &EventKind::VerifyPassed {
                    attempt: AttemptId::new(1),
                },
            )
            .expect("append 2");

        let err = journal
            .rebuild_state()
            .expect_err("must reject the invalid transition instead of stopping silently");

        let message = err.to_string();
        assert!(
            message.contains(&bad_seq.to_string()),
            "expected the error to name the offending seq {bad_seq}, got {message:?}"
        );
        assert!(matches!(err, Error::Corrupt { .. }));
    }

    #[test]
    fn rebuild_state_leaves_existing_projection_untouched_when_replay_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let good_task = TaskId::new(1);
        journal
            .put_state(good_task, &TaskState::Preflight)
            .expect("seed good_task state");

        let bad_task = TaskId::new(2);
        journal
            .append(
                Some(bad_task),
                &EventKind::TaskQueued {
                    title: "B".to_string(),
                },
            )
            .expect("append 1");
        journal
            .append(
                Some(bad_task),
                &EventKind::VerifyPassed {
                    attempt: AttemptId::new(1),
                },
            )
            .expect("append 2");

        journal
            .rebuild_state()
            .expect_err("replay must fail on the invalid transition");

        assert_eq!(
            journal.get_state(good_task).expect("get_state"),
            Some(TaskState::Preflight),
            "a failed rebuild must not clear or partially rewrite the existing projection"
        );
    }

    #[test]
    fn for_each_event_visits_all_ten_thousand_events_via_callback_count() {
        const TOTAL: u64 = 10_000;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        {
            let tx = journal.conn.transaction().expect("tx");
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO events (ts, task_id, kind, payload) \
                         VALUES ('2024-01-01T00:00:00Z', NULL, 'Resumed', '{\"kind\":\"Resumed\"}')",
                    )
                    .expect("prepare");
                for _ in 0..TOTAL {
                    stmt.execute([]).expect("insert");
                }
            }
            tx.commit().expect("commit");
        }

        // The callback counts events instead of storing them: this is what
        // proves the ten-thousand-row journal was streamed row by row
        // rather than collected into a `Vec` first.
        let mut count: u64 = 0;
        journal
            .for_each_event(EventSeq::new(0), &mut |_event| {
                count += 1;
                Ok(())
            })
            .expect("for_each_event");

        assert_eq!(count, TOTAL);
    }

    #[test]
    fn for_each_event_excludes_from_and_everything_before_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let first = journal.append(None, &EventKind::Resumed).expect("append 1");
        let second = journal
            .append(None, &EventKind::PreflightStarted)
            .expect("append 2");
        let third = journal
            .append(
                None,
                &EventKind::TaskCancelled {
                    reason: "later".to_string(),
                },
            )
            .expect("append 3");

        let mut seqs: Vec<EventSeq> = Vec::new();
        journal
            .for_each_event(first, &mut |event| {
                seqs.push(event.seq);
                Ok(())
            })
            .expect("for_each_event");

        assert_eq!(seqs, vec![second, third]);
    }

    #[test]
    fn for_each_event_stops_and_propagates_the_callbacks_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        journal.append(None, &EventKind::Resumed).expect("append 1");
        journal
            .append(None, &EventKind::PreflightStarted)
            .expect("append 2");
        journal.append(None, &EventKind::Resumed).expect("append 3");

        let mut visited = 0;
        let err = journal
            .for_each_event(EventSeq::new(0), &mut |_event| {
                visited += 1;
                Err(Error::Corrupt {
                    detail: "stop here".to_string(),
                })
            })
            .expect_err("callback error must propagate");

        assert!(matches!(&err, Error::Corrupt { detail } if detail == "stop here"));
        assert_eq!(visited, 1, "must stop at the first callback error");
    }

    #[test]
    fn put_tasks_into_a_non_empty_queue_is_refused_naming_the_existing_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = Journal::open(&path).expect("open");

        let first_plan = crate::parse_plan(
            "\
## Already queued

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
",
        )
        .expect("parse_plan");
        journal.put_tasks(&first_plan).expect("first put_tasks");

        let second_plan = crate::parse_plan(
            "\
## A different plan

**Outcome:** something else.

**Done-when:** something else happened.

**Verify:** `true`

**Refs:** none
",
        )
        .expect("parse_plan");
        let err = journal
            .put_tasks(&second_plan)
            .expect_err("importing into a non-empty queue must be refused");

        assert!(
            matches!(&err, Error::Policy { detail, .. } if detail.contains('1')),
            "expected a Policy error naming the existing count of 1, got {err:?}"
        );

        let unchanged = journal.tasks().expect("tasks");
        assert_eq!(
            unchanged, first_plan,
            "a refused import must not touch the existing queue"
        );
    }
}

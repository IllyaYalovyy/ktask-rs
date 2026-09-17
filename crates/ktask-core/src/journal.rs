//! The journal: one SQLite file holding what a run did, plus the state
//! materialized from it.
//!
//! `docs/DESIGN.md` Database schema gives the schema — `events`, `tasks`,
//! `task_state`, `meta`, and one index over `events` — and this module is the
//! only code that creates it or names the file it lives in. [`journal_path`]
//! exists so every other module asks a [`crate::Project`] for its state
//! directory instead of spelling `journal.db`: the filename is one constant,
//! which is what lets registration and [`Journal::open`] work on one database rather
//! than on two that happen to collide (ADR-0013, ADR-0015).
//!
//! # Why these pragmas
//!
//! `journal_mode = WAL` and `synchronous = FULL` are stated on every open, not
//! only at creation. The mode belongs to the file, so the open that set it is
//! enough for later ones; `synchronous` belongs to the connection, so a
//! connection that forgot it would quietly lose its last commit to a power
//! loss — and losing the last event is the exact failure this tool exists to
//! make impossible. `FULL` costs one fsync per commit, and a task commits a
//! handful of events.
//!
//! # The version row
//!
//! `meta` carries a `schema_version` row, which is what makes an older or newer
//! file *legible* rather than merely different. [`Journal::open`] reads it before it
//! writes anything, so a journal written by a later ktask-rs is refused with a
//! message naming both versions instead of being half-upgraded into a shape its
//! author would not recognize. That refusal is deliberately not
//! [`Error::Corrupt`]: the file is fine, this program is the older one, and a
//! human told to treat a healthy journal as damaged goes looking for a backup
//! instead of upgrading the tool (ADR-0015).
//!
//! # Why opening never makes a directory
//!
//! A state directory is created by `register`, at mode `0700`, because what it
//! holds is private (VISION.md section 11). A directory that [`Journal::open`] made on
//! someone's behalf would be `0755` and would look exactly like a registration,
//! so an absent directory is reported as the error it is.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::{Error, Project, Result};

/// The database a project's durable data lives in, below its state directory.
const JOURNAL_DATABASE: &str = "journal.db";

/// The `meta` row that records which schema version the file is on.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// The schema version this build writes, and the newest one it will open.
///
/// Version `1` is the schema exactly as `docs/DESIGN.md` gives it. A file with
/// no version row is version `0`: the shape a registration leaves behind, since
/// writing one row needs the `meta` table and nothing else. Upgrading it is not
/// a step of its own — every object it lacks is created by the same
/// `IF NOT EXISTS` DDL that creates them in a new file.
const SCHEMA_VERSION: i64 = 1;

/// The `events` table, copied from `docs/DESIGN.md` Database schema.
///
/// It is the source of truth: append-only, with no `UPDATE` and no `DELETE`
/// anywhere in this codebase.
const CREATE_EVENTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS events (
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts       TEXT    NOT NULL,          -- RFC 3339, UTC
  task_id  INTEGER,                   -- NULL for queue-level events
  kind     TEXT    NOT NULL,          -- EventKind discriminant
  payload  TEXT    NOT NULL           -- JSON
);";

/// The index that makes "what happened to task N, in order" an indexed read.
const CREATE_EVENT_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id, seq);";

/// The queue itself, which lives in the journal rather than in a file.
const CREATE_TASKS_TABLE: &str = "CREATE TABLE IF NOT EXISTS tasks (
  id         INTEGER PRIMARY KEY,     -- queue position, 1-based
  title      TEXT    NOT NULL,
  outcome    TEXT    NOT NULL,
  done_when  TEXT    NOT NULL,
  verify     TEXT    NOT NULL,
  refs       TEXT    NOT NULL,
  protocol   TEXT,                    -- NULL means the configured default
  body       TEXT    NOT NULL,        -- the block as authored
  added_at   TEXT    NOT NULL
);";

/// The materialized current state: a projection, rebuildable by replay.
const CREATE_TASK_STATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS task_state (
  task_id    INTEGER PRIMARY KEY,
  state_json TEXT    NOT NULL,
  updated_at TEXT    NOT NULL
);";

/// The key/value table the schema version and the registration live in.
///
/// `pub(crate)` because registration writes its `repo_path` row through this
/// same door, and needs the table to exist before the journal is opened
/// (ADR-0013). It is the one statement of that table's DDL either way.
pub(crate) const CREATE_META_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS meta (\n  key   TEXT PRIMARY KEY,\n  value TEXT NOT NULL\n);";

/// Every object the schema owns, in the order `docs/DESIGN.md` lists them.
const SCHEMA: &[&str] = &[
    CREATE_EVENTS_TABLE,
    CREATE_EVENT_INDEX,
    CREATE_TASKS_TABLE,
    CREATE_TASK_STATE_TABLE,
    CREATE_META_TABLE,
];

/// An opened journal database, with its schema in place and its pragmas set.
///
/// The connection stays private on purpose: the journal's guarantees —
/// append-only, replayable, one writer — are kept by the operations this module
/// offers, not by a caller that could run any statement it liked over a raw
/// handle.
#[derive(Debug)]
pub struct Journal {
    conn: Connection,
}

/// The path of the journal database inside one project's state directory.
///
/// The name of a project's durable file is decided here and nowhere else, so a
/// caller holding a state directory and a [`Journal`] cannot disagree about
/// which file they are both talking about.
#[must_use]
pub fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join(JOURNAL_DATABASE)
}

impl Journal {
    /// Open the journal at `path`, creating or upgrading its schema.
    ///
    /// Opening is idempotent: the schema is `CREATE TABLE IF NOT EXISTS`
    /// throughout, the version row is written with the value it already holds,
    /// and the two pragmas are assertions rather than changes once the file is
    /// in WAL mode. A second [`Journal::open`] of the same file therefore adds no
    /// object, no row, and no event sequence number — a journal that has recorded
    /// work keeps what it recorded, including the `AUTOINCREMENT` counter that
    /// keeps a sequence from ever being reused.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the file, or the directory meant to hold it,
    /// cannot be opened — a missing state directory is refused rather than
    /// created, see the module documentation; [`Error::Config`] when the file
    /// records a schema version later than this build's, which is checked before
    /// anything at all is written to it; [`Error::Corrupt`] when the version row
    /// holds something that is not a number.
    pub fn open(path: &Path) -> Result<Journal> {
        let conn = Connection::open(path)?;
        let recorded = recorded_version(&conn, path)?;
        if let Some(version) = recorded.filter(|version| *version > SCHEMA_VERSION) {
            return Err(Error::Config {
                key: SCHEMA_VERSION_KEY.to_owned(),
                detail: format!(
                    "journal `{}` is on schema version {version}, which this ktask-rs build \
                     reads and writes version {SCHEMA_VERSION}; the newer file is left exactly \
                     as it is",
                    path.display()
                ),
            });
        }
        conn.execute_batch("PRAGMA journal_mode = WAL;\nPRAGMA synchronous = FULL;")?;
        for statement in SCHEMA {
            conn.execute_batch(statement)?;
        }
        stamp_version(&conn)?;
        Ok(Journal { conn })
    }

    /// Open the journal of one registered project.
    ///
    /// The project's state directory is already there — `register` made it — so
    /// this is the call every caller above the filesystem layer wants, and the
    /// only one that has to know a project's durable data is a file called
    /// `JOURNAL_DATABASE`: it asks [`journal_path`] and hands the answer to
    /// [`Journal::open`].
    ///
    /// # Errors
    ///
    /// As [`Journal::open`]: [`Error::Database`] when the state directory is not
    /// there or its journal cannot be opened, [`Error::Config`] on a journal from
    /// a later schema version, [`Error::Corrupt`] on an unreadable version row.
    pub fn open_for(project: &Project) -> Result<Journal> {
        Self::open(&journal_path(&project.state_dir))
    }

    /// The schema version the file is recorded at, read back from `meta` rather
    /// than remembered from the call that opened it.
    ///
    /// What a journal *is* comes from the file, not from the build that happens
    /// to be reading it: a caller comparing this against its own
    /// `SCHEMA_VERSION` is asking the durable record a question, not echoing a
    /// constant back.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when `meta` can no longer be read, [`Error::Corrupt`]
    /// when the row this call wrote at open time is gone or is not a number.
    pub fn schema_version(&self) -> Result<i64> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![SCHEMA_VERSION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let Some(recorded) = stored else {
            return Err(Error::Corrupt {
                detail: format!(
                    "an open journal has no `{SCHEMA_VERSION_KEY}` row in `meta`, so the schema \
                     it was opened against is not the one it is claiming"
                ),
                seq: None,
            });
        };
        parse_version(&recorded).ok_or_else(|| Error::Corrupt {
            detail: format!(
                "`{SCHEMA_VERSION_KEY}` holds `{recorded}`, which is not a schema version"
            ),
            seq: None,
        })
    }
}

/// The schema version a file records, or `None` when it records none.
///
/// Absence is a real state rather than an error: a journal database that
/// registration created holds `meta` and one row, and is version `0` — the one
/// version this build upgrades *from*, because everything it lacks is created by
/// the same DDL a new file gets.
///
/// # Errors
///
/// [`Error::Database`] when `meta` exists and cannot be read, [`Error::Corrupt`]
/// when the row is there and is not a number.
fn recorded_version(conn: &Connection, path: &Path) -> Result<Option<i64>> {
    if !table_is_there(conn, "meta")? {
        return Ok(None);
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![SCHEMA_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    parse_version(&stored)
        .map(Some)
        .ok_or_else(|| Error::Corrupt {
            detail: format!(
                "journal `{}` records `{SCHEMA_VERSION_KEY}` `{stored}`, which is not a schema \
             version, so its age cannot be established",
                path.display()
            ),
            seq: None,
        })
}

/// Whether `name` is one of the tables the file already holds.
///
/// Asked of `sqlite_master` rather than by running a statement against the table
/// and reading the error: a missing table and a table that refuses a query are
/// different answers, and only one of them is version `0`.
fn table_is_there(conn: &Connection, name: &str) -> Result<bool> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get::<_, i64>(0),
    )
    .map(|found| found > 0)
    .map_err(Into::into)
}

/// Write the version row, or leave it holding the value it already holds.
///
/// Written every time rather than only when absent, because an idempotent call
/// that repairs an interrupted one is what lets `ktask-rs init` be re-run by
/// someone who cannot remember running it: the value is the same value, so no
/// durable fact changes.
fn stamp_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION_KEY, SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// The number `stored` holds, or `None` when it holds anything else.
///
/// A `+1` is a version as much as a `1` is, so the parse is the strict one and
/// the caller decides what an unreadable version means.
fn parse_version(stored: &str) -> Option<i64> {
    stored.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::{CREATE_META_TABLE, Journal, SCHEMA_VERSION, SCHEMA_VERSION_KEY, journal_path};
    use crate::{Error, Project};
    use rusqlite::{Connection, OptionalExtension as _, params};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::{TempDir, tempdir};

    /// Every object a journal owns, as `sqlite_master` describes it, ordered by
    /// object name the way the test query orders them.
    const SCHEMA_OBJECTS: [&str; 5] = [
        "table events",
        "index idx_events_task",
        "table meta",
        "table task_state",
        "table tasks",
    ];

    /// The `ts` column wants an RFC 3339 instant; which one is nobody's business.
    const AN_INSTANT: &str = "2026-09-17T12:00:00+00:00";

    /// A scratch parent for a journal, below the system temp directory:
    /// `docs/DESIGN.md` Conventions forbids a test from writing in here.
    fn scratch() -> TempDir {
        tempdir().expect("a scratch directory below the system temp directory")
    }

    /// The journal file of one scratch directory, spelled the way a caller who
    /// has never heard of this module would spell it.
    fn journal_file(state_dir: &Path) -> PathBuf {
        state_dir.join("journal.db")
    }

    /// Everything the file owns, as `type name`.
    fn objects(conn: &Connection) -> Vec<String> {
        let mut statement = conn
            .prepare(
                "SELECT type || ' ' || name FROM sqlite_master \
                 WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("sqlite_master is always readable");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("sqlite_master is readable")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("every object name is text")
    }

    /// A table's columns as (name, declared type, `NOT NULL`, primary-key
    /// position) — the four facts `docs/DESIGN.md` states about each column.
    fn columns(conn: &Connection, table: &str) -> Vec<(String, String, i64, i64)> {
        let mut statement = conn
            .prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1) ORDER BY cid")
            .expect("table_info is always readable");
        statement
            .query_map(params![table], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("table_info is readable")
            .collect::<rusqlite::Result<Vec<(String, String, i64, i64)>>>()
            .expect("every column is described in text and numbers")
    }

    /// The indexes on one table, by name.
    fn indexes_on(conn: &Connection, table: &str) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT name FROM pragma_index_list(?1) ORDER BY name")
            .expect("index_list is always readable");
        statement
            .query_map(params![table], |row| row.get::<_, String>(0))
            .expect("index_list is readable")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("every index is named in text")
    }

    /// The columns one index covers, in the order it covers them.
    fn index_columns(conn: &Connection, index: &str) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
            .expect("index_info is always readable");
        statement
            .query_map(params![index], |row| row.get::<_, String>(0))
            .expect("index_info is readable")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("every covered column is named in text")
    }

    /// The value a `meta` row holds, if that row is there.
    fn meta_value(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .expect("the `meta` table is readable")
    }

    /// How many rows `meta` holds.
    fn meta_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM meta", [], |row| row.get(0))
            .expect("the `meta` table is readable")
    }

    /// The sequence numbers `events` holds, oldest first.
    fn sequences(conn: &Connection) -> Vec<i64> {
        let mut statement = conn
            .prepare("SELECT seq FROM events ORDER BY seq")
            .expect("events is always readable");
        statement
            .query_map([], |row| row.get(0))
            .expect("events is readable")
            .collect::<rusqlite::Result<Vec<i64>>>()
            .expect("every sequence is a number")
    }

    /// Append one event, the way the append operation will.
    fn append_event(conn: &Connection, kind: &str) {
        conn.execute(
            "INSERT INTO events (ts, task_id, kind, payload) VALUES (?1, NULL, ?2, ?3)",
            params![AN_INSTANT, kind, "{}"],
        )
        .expect("an event appends");
    }

    /// A journal database holding the `meta` table, one `schema_version` row, and
    /// nothing else — a file this build did not open.
    fn journal_at_version(path: &Path, version: &str) {
        let conn = Connection::open(path).expect("a scratch database");
        conn.execute_batch(CREATE_META_TABLE)
            .expect("a scratch `meta` table");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            params![SCHEMA_VERSION_KEY, version],
        )
        .expect("a scratch schema version row");
    }

    /// A project whose state directory is `state_dir`, assembled rather than
    /// registered: registration needs a working copy and an environment, and none
    /// of that is what this test is about.
    fn project_with_state_dir(state_dir: PathBuf) -> Project {
        Project {
            id: "1a2b3c4d5e6f7a8b".to_owned(),
            root: state_dir
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
            state_dir,
        }
    }

    #[test]
    fn journal_path_names_the_journal_database_below_the_state_directory() {
        assert_eq!(
            journal_path(Path::new("/state/1a2b3c4d5e6f7a8b")),
            PathBuf::from("/state/1a2b3c4d5e6f7a8b/journal.db")
        );
    }

    #[test]
    fn opening_a_missing_file_creates_the_journal_it_describes() {
        let parent = scratch();
        let path = journal_file(parent.path());

        let journal = Journal::open(&path).expect("opening a missing journal creates it");

        assert_eq!(
            objects(&journal.conn),
            SCHEMA_OBJECTS,
            "the three tables and the index of `docs/DESIGN.md` Database schema, and nothing \
             else, all created the first time"
        );
        assert!(
            path.is_file(),
            "the file is there for the next start to find"
        );
    }

    #[test]
    fn the_events_table_holds_the_columns_the_design_gives_it() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        assert_eq!(
            columns(&journal.conn, "events"),
            [
                ("seq".to_owned(), "INTEGER".to_owned(), 0, 1),
                ("ts".to_owned(), "TEXT".to_owned(), 1, 0),
                ("task_id".to_owned(), "INTEGER".to_owned(), 0, 0),
                ("kind".to_owned(), "TEXT".to_owned(), 1, 0),
                ("payload".to_owned(), "TEXT".to_owned(), 1, 0),
            ],
            "`seq` is the rowid alias and every column the design marks NOT NULL is that way"
        );
    }

    #[test]
    fn the_queue_and_projection_tables_hold_the_columns_the_design_gives_them() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        assert_eq!(
            columns(&journal.conn, "tasks"),
            [
                ("id".to_owned(), "INTEGER".to_owned(), 0, 1),
                ("title".to_owned(), "TEXT".to_owned(), 1, 0),
                ("outcome".to_owned(), "TEXT".to_owned(), 1, 0),
                ("done_when".to_owned(), "TEXT".to_owned(), 1, 0),
                ("verify".to_owned(), "TEXT".to_owned(), 1, 0),
                ("refs".to_owned(), "TEXT".to_owned(), 1, 0),
                ("protocol".to_owned(), "TEXT".to_owned(), 0, 0),
                ("body".to_owned(), "TEXT".to_owned(), 1, 0),
                ("added_at".to_owned(), "TEXT".to_owned(), 1, 0),
            ],
            "`protocol` is the only nullable column, because NULL means the configured default"
        );
        assert_eq!(
            columns(&journal.conn, "task_state"),
            [
                ("task_id".to_owned(), "INTEGER".to_owned(), 0, 1),
                ("state_json".to_owned(), "TEXT".to_owned(), 1, 0),
                ("updated_at".to_owned(), "TEXT".to_owned(), 1, 0),
            ],
            "one projected row per task, keyed by the task"
        );
        assert_eq!(
            columns(&journal.conn, "meta"),
            [
                ("key".to_owned(), "TEXT".to_owned(), 0, 1),
                ("value".to_owned(), "TEXT".to_owned(), 1, 0),
            ],
            "a key/value table whose key cannot be missing"
        );
    }

    #[test]
    fn a_lost_tail_event_never_buys_its_sequence_back() {
        let parent = scratch();
        let path = journal_file(parent.path());

        {
            let opened = Journal::open(&path).expect("a new journal");
            append_event(&opened.conn, "TaskQueued");
            append_event(&opened.conn, "PreflightStarted");
            // Staging a lost tail record — what an interrupted commit leaves
            // behind. Production code never deletes from `events`; a test does it
            // here because the loss is the situation being tested. `AUTOINCREMENT`
            // is what keeps the next record from claiming a sequence a run has
            // already reported to a human.
            opened
                .conn
                .execute("DELETE FROM events WHERE seq = 2", [])
                .expect("the tail record can be lost");
        }
        let reopened = Journal::open(&path).expect("the journal reopens");
        append_event(&reopened.conn, "PreflightPassed");

        assert_eq!(
            sequences(&reopened.conn),
            [1, 3],
            "sequence 2 is spent for good: a journal whose numbering repeats would read two \
             records as one thing"
        );
    }

    #[test]
    fn the_index_covers_task_then_sequence_so_one_tasks_events_stay_ordered() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        assert_eq!(indexes_on(&journal.conn, "events"), ["idx_events_task"]);
        assert_eq!(
            index_columns(&journal.conn, "idx_events_task"),
            ["task_id", "seq"]
        );
    }

    #[test]
    fn the_journal_is_opened_in_wal_mode_with_synchronous_full() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        let mode: String = journal
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("the journal mode is always readable");
        let synchronous: i64 = journal
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("the synchronous level is always readable");

        assert_eq!(mode, "wal", "the mode `docs/DESIGN.md` states");
        assert_eq!(
            synchronous, 2,
            "FULL is 2: a commit returns after the fsync"
        );
    }

    #[test]
    fn the_journal_stamps_the_version_row_that_makes_its_age_legible() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        assert_eq!(
            journal
                .schema_version()
                .expect("the row this open wrote is readable"),
            SCHEMA_VERSION
        );
        assert_eq!(
            meta_value(&journal.conn, SCHEMA_VERSION_KEY).as_deref(),
            Some("1"),
            "`meta` is a text table, and version 1 is the schema as written"
        );
    }

    #[test]
    fn opening_the_same_journal_twice_adds_no_object_and_no_row() {
        let parent = scratch();
        let path = journal_file(parent.path());

        {
            let first = Journal::open(&path).expect("a new journal opens");
            assert_eq!(objects(&first.conn), SCHEMA_OBJECTS);
        }
        let second = Journal::open(&path).expect("opening a journal that exists is not an error");

        assert_eq!(objects(&second.conn), SCHEMA_OBJECTS);
        assert_eq!(
            meta_rows(&second.conn),
            1,
            "the version row is written with the value it already holds, so a second open adds \
             nothing to `meta`"
        );
        assert_eq!(
            second.schema_version().expect("the row is still readable"),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn a_reopened_journal_never_reuses_an_event_sequence() {
        let parent = scratch();
        let path = journal_file(parent.path());

        {
            let first = Journal::open(&path).expect("a new journal");
            append_event(&first.conn, "TaskQueued");
            append_event(&first.conn, "PreflightStarted");
            assert_eq!(sequences(&first.conn), [1, 2]);
        }
        let second = Journal::open(&path).expect("the journal reopens");
        append_event(&second.conn, "PreflightPassed");

        assert_eq!(
            sequences(&second.conn),
            [1, 2, 3],
            "the events already recorded survive, and `AUTOINCREMENT` continues rather than \
             restarting: a reused sequence would make two records of one thing"
        );
    }

    #[test]
    fn a_journal_recorded_at_an_older_version_is_carried_forward_to_this_one() {
        let parent = scratch();
        let path = journal_file(parent.path());
        journal_at_version(&path, "0");

        let opened =
            Journal::open(&path).expect("an older journal is carried forward, not refused");

        assert_eq!(objects(&opened.conn), SCHEMA_OBJECTS);
        assert_eq!(
            meta_rows(&opened.conn),
            1,
            "the one row it had is still the one row it has"
        );
        assert_eq!(
            opened
                .schema_version()
                .expect("the row this open wrote is readable"),
            SCHEMA_VERSION,
            "the row says what the file holds after this open, not what it held before it"
        );
    }

    #[test]
    fn a_journal_from_a_later_schema_version_is_refused_rather_than_upgraded_downward() {
        let parent = scratch();
        let path = journal_file(parent.path());
        journal_at_version(&path, "2");

        let error = Journal::open(&path).expect_err("a journal from a later version cannot open");

        assert!(
            !matches!(error, Error::Corrupt { .. }),
            "the file is healthy and this build is the older one, so calling it corruption \
             sends a human hunting for a backup instead of upgrading: {error}"
        );
        let Error::Config { key, detail } = error else {
            panic!("a future schema version is a version question, not {error}");
        };
        assert_eq!(key, SCHEMA_VERSION_KEY);
        assert!(
            detail.contains("schema version 2") && detail.contains("writes version 1"),
            "the message names both versions, so the operator knows which of the two is newer: \
             {detail}"
        );
    }

    #[test]
    fn the_refusal_of_a_later_version_names_the_file_it_refused() {
        let parent = scratch();
        let path = journal_file(parent.path());
        journal_at_version(&path, "7");

        let error = Journal::open(&path).expect_err("a journal from a later version cannot open");

        assert!(
            error.to_string().contains(&path.display().to_string()),
            "one state root holds many project journals, so the message has to say which file: \
             {error}"
        );
    }

    #[test]
    fn refusing_a_later_version_writes_nothing_into_the_newer_file() {
        let parent = scratch();
        let path = journal_file(parent.path());
        journal_at_version(&path, "2");

        let refused = Journal::open(&path).expect_err("a journal from a later version cannot open");
        assert!(refused.to_string().contains("left exactly as it is"));

        let unchanged = Connection::open(&path).expect("the refused journal is still readable");
        assert_eq!(
            objects(&unchanged),
            ["table meta"],
            "the version is read before any DDL runs, so a refusal cannot half-upgrade a file \
             this build does not understand"
        );
        assert_eq!(
            meta_value(&unchanged, SCHEMA_VERSION_KEY).as_deref(),
            Some("2"),
            "the version this build refused stays recorded, because the newer tool that wrote it \
             is the one that will read it again"
        );
    }

    #[test]
    fn a_version_row_that_is_not_a_number_is_refused_as_corruption() {
        let parent = scratch();
        let path = journal_file(parent.path());
        journal_at_version(&path, "two");

        let error = Journal::open(&path).expect_err("an unreadable version cannot be trusted");

        let Error::Corrupt { detail, seq } = error else {
            panic!("durable data that cannot be read is corruption, not {error}");
        };
        assert!(
            detail.contains("two"),
            "the message quotes what it found: {detail}"
        );
        assert!(seq.is_none(), "no journal record is implicated here");
    }

    #[test]
    fn opening_upgrades_a_registration_database_without_losing_its_row() {
        let parent = scratch();
        let path = journal_file(parent.path());
        {
            let registered = Connection::open(&path).expect("the database registration makes");
            registered
                .execute_batch(CREATE_META_TABLE)
                .expect("registration creates `meta` and nothing else");
            registered
                .execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                    params!["repo_path", "/repository"],
                )
                .expect("registration records the repository");
        }

        let opened =
            Journal::open(&path).expect("a versionless database is version 0, which opens");

        assert_eq!(
            objects(&opened.conn),
            SCHEMA_OBJECTS,
            "every object a registration did not make arrives on the first real open"
        );
        assert_eq!(
            meta_value(&opened.conn, "repo_path").as_deref(),
            Some("/repository"),
            "an upgrade adds; it never drops what the file already recorded"
        );
        assert_eq!(
            opened
                .schema_version()
                .expect("the row this open wrote is readable"),
            SCHEMA_VERSION
        );
        assert_eq!(meta_rows(&opened.conn), 2);
    }

    #[test]
    fn open_for_opens_the_journal_below_the_projects_state_directory() {
        let parent = scratch();
        let state_dir = parent.path().join("1a2b3c4d5e6f7a8b");
        fs::create_dir(&state_dir).expect("a scratch state directory");
        let project = project_with_state_dir(state_dir.clone());

        let journal = Journal::open_for(&project).expect("a registered project has a journal");

        assert!(
            state_dir.join("journal.db").is_file(),
            "the project's durable data is one file below its own state directory"
        );
        assert_eq!(
            journal
                .schema_version()
                .expect("the row this open wrote is readable"),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn opening_refuses_a_state_directory_that_was_never_made() {
        let parent = scratch();
        let missing = parent.path().join("never-registered");

        let error = Journal::open(&journal_file(&missing))
            .expect_err("opening does not invent a state directory");

        assert!(
            matches!(error, Error::Database(_)),
            "the filesystem refused the open and says so: {error}"
        );
        assert!(
            !missing.exists(),
            "a directory made here would be 0755 and would look exactly like a registration: \
             making it, at 0700, is `register`'s job (VISION.md section 11)"
        );
    }

    #[test]
    fn a_version_row_lost_after_opening_is_reported_as_corruption() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        journal
            .conn
            .execute("DELETE FROM meta", [])
            .expect("the row this build wrote can be removed from under it");

        let error = journal
            .schema_version()
            .expect_err("a journal that stopped recording its version is not legible");

        assert!(
            matches!(error, Error::Corrupt { .. }),
            "the row is this open's own, so its absence is damage, not a version question: \
             {error}"
        );
    }
}

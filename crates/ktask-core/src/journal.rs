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
//!
//! # Appending
//!
//! [`Journal::append`] is the only write the journal makes, and it is where
//! invariant 3 of VISION.md section 3 becomes mechanical. Three values make a row
//! and none of them comes from the caller: the sequence is the database's own
//! `AUTOINCREMENT` counter, the instant is the clock read inside the call, and the
//! payload is `serde_json`'s encoding of the catalog entry. A sequence a caller
//! chose is a sequence two callers can choose, and an instant handed in from
//! outside is one a caller can move — in a record whose whole purpose is to say
//! what a run did, and when.
//!
//! One insert is one transaction, so "persisted atomically before it takes
//! effect" is the transaction's guarantee rather than a promise: either the event
//! is in the file with a sequence of its own, or the journal holds precisely what
//! it held before the call, with no sequence number spent.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::ser::Error as _;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{Error, EventKind, EventSeq, Project, Result, TaskId};

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

/// The nanoseconds in one second, so a clock reading keeps the part of itself
/// below a second instead of rounding it away.
const NANOSECONDS_PER_SECOND: i128 = 1_000_000_000;

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

    /// Append one event to the journal and return the sequence it was written
    /// at.
    ///
    /// Nothing the row is made of is supplied by the caller except *what
    /// happened*: `seq` comes from the database's `AUTOINCREMENT` counter, `ts`
    /// from the clock read inside this call, and `payload` from `serde_json`.
    /// That is why the signature takes `&mut self` and no timestamp — a sequence
    /// a caller chose is a sequence two callers can choose, and an instant handed
    /// in from outside is one a caller can move, in the one record that exists to
    /// say what a run did and when (VISION.md section 3, invariant 3).
    ///
    /// The `kind` column gets [`EventKind::discriminant`] and the `payload`
    /// column gets the tagged object whose own `kind` tag carries that same
    /// string. The column is what an indexed `WHERE` reads without parsing JSON;
    /// a test over every catalog entry keeps the two halves honest against each
    /// other, rather than a comment promising they are.
    ///
    /// # Errors
    ///
    /// [`Error::Serde`] when the payload has no JSON encoding, or when the clock
    /// reads an instant with no RFC 3339 spelling; [`Error::Database`] when
    /// SQLite refuses the insert. Either way the journal holds exactly what it
    /// held before the call and no sequence number has been spent, because the
    /// encoding happens before a transaction opens and a refused insert is
    /// rolled back rather than left for a replay to trip over.
    pub fn append(&mut self, task_id: Option<TaskId>, kind: &EventKind) -> Result<EventSeq> {
        self.append_encoded(task_id, kind.discriminant(), || {
            serde_json::to_string(kind).map_err(Into::into)
        })
    }

    /// The append pipeline with its payload step handed in by the caller.
    ///
    /// [`Journal::append`] supplies `serde_json`; a test supplies a serializer
    /// that refuses, which is the only way to watch the first guarantee below
    /// hold — no field of [`EventKind`] is one `serde_json` refuses today, and
    /// everything else the two calls do is the same code, so the test measures
    /// the pipeline rather than a copy of it.
    ///
    /// The order of the steps *is* the atomicity the invariant asks for:
    ///
    /// 1. encode the payload, then stamp the instant;
    /// 2. open a transaction;
    /// 3. insert, reading back the sequence the row was given;
    /// 4. accept that sequence, or roll back;
    /// 5. commit.
    ///
    /// A refusal at step 1 never reaches the database, so nothing is written and
    /// no sequence number is spent. A refusal at any later step drops the
    /// transaction without committing it, which `rusqlite` rolls back.
    fn append_encoded(
        &mut self,
        task_id: Option<TaskId>,
        kind: &str,
        encode: impl FnOnce() -> Result<String>,
    ) -> Result<EventSeq> {
        let payload = encode()?;
        let ts = stamp_text(clock_nanos())?;
        let transaction = self.conn.transaction()?;
        let written: i64 = transaction.query_row(
            "INSERT INTO events (ts, task_id, kind, payload) VALUES (?1, ?2, ?3, ?4) \
             RETURNING seq",
            params![ts, task_id.map(|id| i64::from(id.get())), kind, payload],
            |row| row.get(0),
        )?;
        // Read back rather than trusted: `seq` is the rowid the row was given,
        // which is the only number in the file the journal can promise is unique.
        // Checked before the commit, so a sequence this build cannot name rolls
        // its own insert back rather than being reported and later contradicted
        // by the file.
        let seq = event_sequence(written)?;
        transaction.commit()?;
        Ok(seq)
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

/// The clock, read now, in nanoseconds since the unix epoch.
///
/// The only line in the crate that reads a clock, and it decides nothing: the
/// shape `docs/QUALITY.md` asks for, so every decision about a timestamp lives
/// above a call a headless test cannot reach. Setting a machine's clock is not
/// something a test may do, so the reading a test wants is the argument of
/// [`reading_nanos`] rather than a stub of this function.
fn clock_nanos() -> i128 {
    reading_nanos(SystemTime::now())
}

/// One clock reading as nanoseconds since the unix epoch, signed.
///
/// `i128` is wide enough that no reading hardware can produce overflows it. The
/// sign is the decision worth a test: `duration_since` reports the *distance*
/// from the epoch whichever way the reading lies, so a clock set before the
/// epoch has to keep the direction it was read with — otherwise a stamp says
/// 1970 about an instant in 1969, in the one record that exists to say when
/// something happened.
fn reading_nanos(reading: SystemTime) -> i128 {
    match reading.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(forward) => nanos(forward),
        Err(went_backwards) => -nanos(went_backwards.duration()),
    }
}

/// A span either side of the epoch, in nanoseconds.
fn nanos(span: Duration) -> i128 {
    i128::from(span.as_secs()) * NANOSECONDS_PER_SECOND + i128::from(span.subsec_nanos())
}

/// The `ts` text for one clock reading, in the one spelling the column
/// documents: RFC 3339, in UTC.
///
/// `docs/DESIGN.md` Conventions fix that spelling, and ADR-0012 decided that an
/// instant with no such text is a *serialization* failure — which is what the two
/// refusals below return instead of a panic: a clock outside the range `time`
/// represents, and one whose year RFC 3339 has no digits for. Both are reachable
/// on a machine whose clock has been set absurdly, and a supervisor that panics
/// loses the run it was supervising.
///
/// The instant is built at the UTC offset, so the conversion ADR-0012 adds for an
/// envelope authored at a local offset has nothing to do here; the format is the
/// `Rfc3339` one `time::serde::rfc3339` uses itself, so the column and the
/// envelope's `ts` field cannot disagree about one event.
fn stamp_text(since_epoch: i128) -> Result<String> {
    let instant = OffsetDateTime::from_unix_timestamp_nanos(since_epoch)
        .map_err(|range| no_text(since_epoch, &range))?;
    instant
        .format(&Rfc3339)
        .map_err(|reason| no_text(since_epoch, &reason))
        .map_err(Into::into)
}

/// The refusal [`stamp_text`] hands back: the reading it could not write, quoted,
/// and the reason it had no spelling.
fn no_text(since_epoch: i128, reason: &dyn Display) -> serde_json::Error {
    serde_json::Error::custom(format_args!(
        "the clock reading {since_epoch} ns since the epoch has no RFC 3339 spelling to store: \
         {reason}"
    ))
}

/// The [`EventSeq`] for the number `events` handed back, or the corruption
/// report for a number that is not a sequence.
///
/// `seq` is an SQLite `INTEGER`, so it is signed, while a sequence number is a
/// count of events that only ever grows. A negative one is therefore not a
/// sequence but a file that no longer agrees with the schema it claims: the
/// journal is the source of truth, so the disagreement is reported rather than
/// quietly widened into a huge number that would look like a legitimate far
/// future event.
fn event_sequence(written: i64) -> Result<EventSeq> {
    u64::try_from(written)
        .map(EventSeq::new)
        .map_err(|out_of_range| Error::Corrupt {
            detail: format!(
                "`events` handed back the sequence {written}, which is not one: {out_of_range}"
            ),
            seq: None,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        CREATE_META_TABLE, EventSeq, Journal, NANOSECONDS_PER_SECOND, SCHEMA_VERSION,
        SCHEMA_VERSION_KEY, event_sequence, journal_path, reading_nanos, stamp_text,
    };
    use crate::{
        AttemptId, Error, EventKind, FailureClass, PauseReason, Phase, Project, Recovery, Result,
        Stream, TaskId,
    };
    use rusqlite::{Connection, OptionalExtension as _, params};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};
    use tempfile::{TempDir, tempdir};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use time::macros::datetime;

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

    /// The commit sha every catalog entry below is built from, so a field that
    /// fails to reach the payload column is visible rather than mistaken for a
    /// placeholder.
    const SHA: &str = "0b78d3f1c2a4";

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

    /// One event as the `events` table holds it, in the types the schema
    /// declares. Read back rather than assumed: every append assertion below is
    /// against what the file ended up carrying.
    #[derive(Debug)]
    struct Stored {
        seq: i64,
        ts: String,
        task_id: Option<i64>,
        kind: String,
        payload: String,
    }

    /// Every event the journal holds, oldest first. The whole table, because
    /// "nothing else was written" is only measurable against all of it.
    fn stored_events(conn: &Connection) -> Vec<Stored> {
        let mut statement = conn
            .prepare("SELECT seq, ts, task_id, kind, payload FROM events ORDER BY seq")
            .expect("events is always readable");
        statement
            .query_map([], |row| {
                Ok(Stored {
                    seq: row.get(0)?,
                    ts: row.get(1)?,
                    task_id: row.get(2)?,
                    kind: row.get(3)?,
                    payload: row.get(4)?,
                })
            })
            .expect("events is readable")
            .collect::<rusqlite::Result<Vec<Stored>>>()
            .expect("every column reads as the type the schema declares it")
    }

    /// The one event `append` returned the sequence of.
    fn stored_at(conn: &Connection, seq: EventSeq) -> Stored {
        let wanted = i64::try_from(seq.get()).expect("a scratch sequence is a positive number");
        stored_events(conn)
            .into_iter()
            .find(|event| event.seq == wanted)
            .unwrap_or_else(|| {
                panic!(
                    "append returned sequence {}, and no row holds it",
                    seq.get()
                )
            })
    }

    /// One value of every catalog entry, so the append contract is measured
    /// against each shape a payload can take — no fields, a string, a number, an
    /// enum, an instant, an optional instant — rather than the easiest one.
    fn every_catalog_entry() -> Vec<EventKind> {
        vec![
            EventKind::TaskQueued {
                title: "Append an event".to_owned(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: SHA.to_owned(),
            },
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "no remote configured".to_owned(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(2),
                protocol: "tdd".to_owned(),
                pid: 42_424,
                base_sha: SHA.to_owned(),
            },
            EventKind::PhaseEntered {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            EventKind::AgentOutput {
                attempt: AttemptId::new(2),
                stream: Stream::Stderr,
                text: "cargo nextest run".to_owned(),
            },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(2),
            },
            EventKind::VerifyFailed {
                attempt: AttemptId::new(3),
                class: FailureClass::VerificationFailure,
                detail: "1 test failed".to_owned(),
            },
            EventKind::PublishStarted {
                attempt: AttemptId::new(3),
                candidate_sha: SHA.to_owned(),
            },
            EventKind::PublishVerified {
                commit: SHA.to_owned(),
                remote_sha: SHA.to_owned(),
            },
            EventKind::TaskDone {
                commit: SHA.to_owned(),
            },
            EventKind::TaskFailed {
                class: FailureClass::NeedsInput,
                detail: "the design is unresolved".to_owned(),
            },
            EventKind::TaskCancelled {
                reason: "an operator stopped it".to_owned(),
            },
            EventKind::Paused {
                reason: PauseReason::Limit {
                    until: Some(datetime!(2026-09-17 13:00:00 UTC)),
                },
            },
            EventKind::Resumed,
            EventKind::Interrupted {
                phase: Phase::Publish,
            },
            EventKind::RecoveryDecision {
                decision: Recovery::MarkInterrupted,
                detail: "no attempt was started".to_owned(),
            },
            EventKind::GateAcknowledged {
                by: "operators.name".to_owned(),
                at: datetime!(2026-09-17 12:34:56 UTC),
            },
        ]
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

    #[test]
    fn every_catalog_entry_appends_under_its_own_name_and_carries_its_own_object() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let entries = every_catalog_entry();

        for (index, kind) in entries.iter().enumerate() {
            let expected = u64::try_from(index + 1).expect("a scratch sequence is a small number");
            let seq = journal
                .append(None, kind)
                .expect("every catalog entry appends, whatever fields it carries");

            assert_eq!(
                seq.get(),
                expected,
                "the first event of a journal is sequence 1 and each next one is exactly one \
                 higher, whatever was appended between them"
            );
            let row = stored_at(&journal.conn, seq);
            assert_eq!(
                row.kind,
                kind.discriminant(),
                "the `kind` column holds `EventKind::discriminant()` for seq {}",
                seq.get()
            );
            assert_eq!(
                row.payload,
                serde_json::to_string(kind).expect("a catalog entry encodes"),
                "the payload column holds the tagged object itself, not a wrapper around it"
            );
            let read_back: EventKind = serde_json::from_str(&row.payload)
                .expect("the payload decodes as the catalog it came from");
            assert_eq!(
                &read_back, kind,
                "the stored payload is the entry, not a paraphrase"
            );

            let tagged: serde_json::Value =
                serde_json::from_str(&row.payload).expect("the payload is one JSON object");
            assert_eq!(
                tagged.get("kind").and_then(serde_json::Value::as_str),
                Some(row.kind.as_str()),
                "the column and the tag inside the payload name the same entry, which is what \
                 lets `WHERE kind = …` and a decoded enum agree about one row"
            );
        }

        assert_eq!(
            stored_events(&journal.conn).len(),
            entries.len(),
            "one row per append, and no row that no append wrote"
        );
    }

    #[test]
    fn a_task_event_stores_its_number_and_a_queue_event_stores_null() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        let queue_level = journal
            .append(None, &EventKind::PreflightStarted)
            .expect("an event about the queue itself appends");
        let numbered = journal
            .append(Some(TaskId::new(u32::MAX)), &EventKind::Resumed)
            .expect("an event about a task appends");

        assert_eq!(
            stored_at(&journal.conn, queue_level).task_id,
            None,
            "a queue-level event is the `task_id` NULL the schema documents: 0 is a queue \
             position the queue never issues, so storing it would invent a task"
        );
        assert_eq!(
            stored_at(&journal.conn, numbered).task_id,
            Some(i64::from(u32::MAX)),
            "the whole width of a queue position survives the INTEGER column"
        );
    }

    #[test]
    fn the_instant_is_the_clock_read_at_the_append_and_written_in_utc() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        let before = OffsetDateTime::from(SystemTime::now());
        let seq = journal
            .append(
                Some(TaskId::new(7)),
                &EventKind::TaskQueued {
                    title: "one".to_owned(),
                },
            )
            .expect("an event appends");
        let after = OffsetDateTime::from(SystemTime::now());

        let row = stored_at(&journal.conn, seq);
        let stamped = OffsetDateTime::parse(&row.ts, &Rfc3339)
            .expect("the `ts` column holds RFC 3339 text, which is what its comment promises");
        assert!(
            stamped >= before && stamped <= after,
            "the stamp is the clock read inside the call, not a constant or a caller's guess: \
             {row:?} falls outside {before}..={after}"
        );
        assert!(
            row.ts.ends_with('Z'),
            "one spelling for one instant, whatever the machine's own offset is: {}",
            row.ts
        );
    }

    #[test]
    fn the_sequence_keeps_climbing_across_appends_and_across_reopens() {
        let parent = scratch();
        let path = journal_file(parent.path());
        let mut written = Vec::new();

        for _ in 0..3 {
            let mut reopened = Journal::open(&path).expect("the journal reopens");
            for _ in 0..2 {
                let seq = reopened
                    .append(Some(TaskId::new(1)), &EventKind::Resumed)
                    .expect("an event appends");
                written.push(seq.get());
            }
            drop(reopened);
        }

        assert_eq!(
            written,
            [1, 2, 3, 4, 5, 6],
            "every open continues the counter the last one left rather than starting over, so \
             a restarted run never re-uses a sequence an earlier one already reported"
        );
        let reopened = Journal::open(&path).expect("the journal opens a fourth time");
        let rows = stored_events(&reopened.conn);
        assert_eq!(
            rows.iter().map(|event| event.seq).collect::<Vec<i64>>(),
            [1, 2, 3, 4, 5, 6],
            "the table holds every sequence that was returned, in the order they were returned"
        );
    }

    #[test]
    fn a_payload_that_cannot_be_encoded_writes_no_row_and_spends_no_sequence() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let kind = EventKind::TaskQueued {
            title: "one".to_owned(),
        };

        let refused = journal
            .append_encoded(
                Some(TaskId::new(1)),
                kind.discriminant(),
                || -> Result<String> {
                    // A map key that is not a string is a refusal `serde_json`
                    // actually performs — a non-finite float is not, it becomes
                    // `null`. Measured, not remembered, because no field of
                    // [`EventKind`] is a value it refuses today: the test hands
                    // the pipeline a payload step that really fails rather than
                    // one that only looks like it could.
                    let mut unmappable = BTreeMap::new();
                    unmappable.insert(b"payload".to_vec(), 1u8);
                    Ok(serde_json::to_string(&unmappable)?)
                },
            )
            .expect_err("a payload that cannot be encoded cannot be appended");

        assert!(
            matches!(refused, Error::Serde(_)),
            "the refusal is the encoder's own and is reported as it came: {refused}"
        );
        assert!(
            stored_events(&journal.conn).is_empty(),
            "the encoding is finished before a transaction opens, so a refusal leaves a \
             journal that was never touched"
        );

        let next = journal
            .append(None, &EventKind::Resumed)
            .expect("a refusal leaves the journal usable");
        assert_eq!(
            next.get(),
            1,
            "the refused event spent no sequence number, so the first event of this journal is \
             still its first"
        );
    }

    #[test]
    fn a_refused_insert_rolls_back_and_leaves_the_committed_events_alone() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let kept = journal
            .append(
                None,
                &EventKind::TaskQueued {
                    title: "kept".to_owned(),
                },
            )
            .expect("the first event appends");
        journal
            .conn
            .execute(
                "CREATE TRIGGER refuse_insert BEFORE INSERT ON events \
                 BEGIN SELECT RAISE(ABORT, 'the test refuses the row'); END",
                [],
            )
            .expect("a trigger can be put on the table by a test");

        let refused = journal
            .append(None, &EventKind::PreflightStarted)
            .expect_err("an insert the database refuses cannot be appended");

        assert!(
            matches!(refused, Error::Database(_)),
            "SQLite's own refusal is passed through as it came: {refused}"
        );
        let rows = stored_events(&journal.conn);
        assert_eq!(rows.len(), 1, "the refused event left no row behind");
        assert_eq!(
            rows.first().expect("the one committed row").kind,
            "TaskQueued",
            "the event that did commit is still there, unchanged"
        );

        journal
            .conn
            .execute("DROP TRIGGER refuse_insert", [])
            .expect("the test removes what it added");
        let after = journal
            .append(None, &EventKind::Resumed)
            .expect("a rolled-back append does not poison the connection");
        assert!(
            after.get() > kept.get(),
            "the rolled-back record took no number back and got a fresh one: {kept} then {after}"
        );
        assert_eq!(
            stored_events(&journal.conn).len(),
            2,
            "the two events that committed are the two the table holds"
        );
    }

    /// A sequence the file hands back is a number this build has to be able to
    /// name, because it is the number every later reader will quote. The guard
    /// runs on what `RETURNING seq` produced, so the test hands it the two
    /// numbers that decide the rule rather than trying to make SQLite invent a
    /// negative rowid.
    #[test]
    fn a_handed_back_sequence_is_accepted_or_reported_as_damage_by_its_number() {
        assert_eq!(
            event_sequence(1).expect("one is a sequence").get(),
            1,
            "the number the row was given is the sequence the caller is told, unchanged"
        );

        let refused =
            event_sequence(-1).expect_err("a negative count of events is not a count of events");

        assert!(
            matches!(refused, Error::Corrupt { seq: None, .. }),
            "a signed number that cannot be a count is the file disagreeing with the schema it \
             claims, which is damage rather than a database refusal: {refused}"
        );
        assert!(
            refused.to_string().contains("-1"),
            "the report quotes the number it refused, so a reader is not left hunting for it: \
             {refused}"
        );
    }

    /// The stamp is a clock reading, and a headless test may not set a machine's
    /// clock — so the reading is the argument, which is why the side of the epoch
    /// and the sub-second part are decisions above the syscall rather than inside
    /// it (`docs/QUALITY.md`).
    #[test]
    fn a_clock_reading_keeps_its_side_of_the_epoch_and_its_sub_second_part() {
        let a_span = Duration::new(2, 250_000_000);

        assert_eq!(
            reading_nanos(SystemTime::UNIX_EPOCH),
            0,
            "the epoch is neither before nor after itself, so it is neither signed"
        );

        let after = SystemTime::UNIX_EPOCH
            .checked_add(a_span)
            .expect("two seconds past the epoch is a representable reading");
        assert_eq!(
            reading_nanos(after),
            2 * NANOSECONDS_PER_SECOND + NANOSECONDS_PER_SECOND / 4,
            "the part below a second survives the reading: two events a quarter-second apart              are two events, and rounding would make them one"
        );

        let before = SystemTime::UNIX_EPOCH
            .checked_sub(a_span)
            .expect("two seconds before the epoch is a representable reading");
        let signed = reading_nanos(before);
        assert_eq!(
            signed,
            -(2 * NANOSECONDS_PER_SECOND + NANOSECONDS_PER_SECOND / 4),
            "`duration_since` reports the distance whichever way the clock lies, so the side              of the epoch has to be carried over into the number"
        );
        assert!(
            stamp_text(signed)
                .expect("1969 has an RFC 3339 spelling")
                .starts_with("1969-12-31T23:59:57"),
            "a clock behind the epoch is stamped on its own side of it, not as 1970"
        );
    }

    #[test]
    fn a_clock_reading_is_stamped_as_the_one_utc_spelling_the_column_documents() {
        assert_eq!(
            stamp_text(datetime!(2026-09-17 12:34:56 UTC).unix_timestamp_nanos())
                .expect("an ordinary instant has a spelling"),
            "2026-09-17T12:34:56Z"
        );
        assert_eq!(
            stamp_text(0).expect("the epoch itself has a spelling"),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn a_clock_reading_beyond_the_instants_this_crate_holds_is_refused_not_panicked_on() {
        let beyond = NANOSECONDS_PER_SECOND * 400_000_000_000;

        let refused = stamp_text(-beyond)
            .expect_err("a clock twelve thousand years before the epoch names no instant");

        assert!(
            matches!(refused, Error::Serde(_)),
            "a stamp that cannot be written is a serialization failure, which is what keeps \
             the append from panicking: {refused}"
        );
        assert!(
            refused.to_string().contains(&(-beyond).to_string()),
            "the message quotes the reading it refused: {refused}"
        );
    }

    #[test]
    fn a_clock_reading_rfc_3339_has_no_digits_for_is_refused_not_panicked_on() {
        let old = NANOSECONDS_PER_SECOND * 66_000_000_000;

        let refused = stamp_text(-old).expect_err(
            "a clock in a negative year is a real instant with no four digits to write",
        );

        assert!(
            matches!(refused, Error::Serde(_)),
            "the refusal is a value, not a panic: {refused}"
        );
        assert!(
            refused.to_string().contains("RFC 3339"),
            "the message names the format that refused, so the reader knows which rule the \
             clock broke: {refused}"
        );
    }
}

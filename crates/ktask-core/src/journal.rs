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
//! [`Journal::append`] writes the only rows a run is judged on — opening the
//! journal writes schema and a version row, but nothing about what happened — and
//! it is where invariant 3 of VISION.md section 3 becomes mechanical. Three
//! values make a row
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
//!
//! # Reading
//!
//! [`Journal::events`], [`Journal::events_for`] and [`Journal::events_since`] hand
//! back [`Event`] records, and all three order them by `seq`: the order the journal
//! means when it says what happened. The instant is stored because a run has to be
//! datable, and two events can share one — so it dates a record and never orders
//! one. A read ordered by `ts` could hand a phase back before the attempt that
//! entered it, which is the journal contradicting itself in the one way a replay
//! would not notice (ADR-0018).
//!
//! Each filter is the SQL's rather than a loop over the rows it picked, so the
//! per-task read walks `idx_events_task` and a cursor read walks the primary key:
//! a reader that polls on a tick never reads what it has already seen twice. A row
//! that cannot be decoded stops the read rather than being skipped — a journal with
//! a hole silently cut out of it is worse to replay than one that says it could not
//! be read.
//!
//! Three of the four reads answer with a [`Vec`], and the fourth —
//! [`Journal::for_each_event`] — answers by handing records to its caller one at a
//! time, which is what lets a journal bigger than the reader be read at all. The
//! four share one statement runner and one row decoder, so a record means the same
//! thing whichever read a caller reached for, and holding a whole journal is a
//! choice a caller makes rather than something a read assumes (ADR-0020).
//!
//! # The materialized state
//!
//! [`Journal::put_state`] writes the state one task is in and
//! [`Journal::get_state`], [`Journal::all_states`] read it back, in `task_state` —
//! the projection `docs/DESIGN.md` Database schema calls rebuildable by replay.
//! Writing it is not recording: the recorder appends the event first and moves the
//! projection second, so no `events` row is inserted and no sequence number is
//! spent, and one row per task means an overwrite rather than a second answer to
//! keep (ADR-0023). A row this build cannot decode is reported as damage rather
//! than as no state, because a projection that quietly said "not started" would
//! hand finished work to a provider again.
//!
//! [`Journal::rebuild_state`] is what makes that rebuildability real: it folds
//! every event through [`crate::apply`] before it touches a row, then clears and
//! refills the table as one transaction, so a journal that cannot be replayed, or
//! a write the database refuses, leaves the projection a run wrote rather than half
//! of a new one (ADR-0024).
//!
//! # Append-only, enforced by the file
//!
//! Two triggers sit on `events` and refuse an `UPDATE` and a `DELETE` with
//! `RAISE(ABORT, …)`, created by the same DDL as the table itself — so
//! "append-only" is a fact about the journal rather than a habit of this module.
//! The refusal comes from SQLite, which means a statement written anywhere (a
//! later crate, a REPL, a recovery tool someone types by hand) is refused the same
//! way, and an open never leaves behind a file missing its guards. `ABORT` undoes
//! the rows a statement had already reached, so even a `DELETE FROM events` naming
//! no row costs the journal nothing.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension as _, Row, ToSql, params};
use serde::ser::Error as _;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{Error, Event, EventKind, EventSeq, Project, Result, TaskId};

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
/// It is the source of truth, and the two guards below are what make it
/// append-only: a rewrite and a removal are refused by the file itself, not merely
/// absent from this codebase.
const CREATE_EVENTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS events (
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts       TEXT    NOT NULL,          -- RFC 3339, UTC
  task_id  INTEGER,                   -- NULL for queue-level events
  kind     TEXT    NOT NULL,          -- EventKind discriminant
  payload  TEXT    NOT NULL           -- JSON
);";

/// The guard that refuses to let a stored event be rewritten.
///
/// `BEFORE`, so the statement is refused before SQLite has written any of it, and
/// `RAISE(ABORT, …)`, so a statement that had already reached some rows leaves
/// none of them changed. The text is what a caller reads — a human in a REPL, or a
/// later crate that reaches for the wrong statement — so it names the rule the
/// statement broke rather than reporting a bare error.
const CREATE_EVENT_UPDATE_TRIGGER: &str =
    "CREATE TRIGGER IF NOT EXISTS events_refuse_update BEFORE UPDATE ON events BEGIN
  SELECT RAISE(ABORT, 'the ktask journal is append-only: an events row is never updated');
END;";

/// The guard that refuses to take a stored event back out of the file.
///
/// The same refusal for the other half of mutation, because an event that vanishes
/// is as much a rewritten history as an event rewritten: what a run did is known
/// only from what the journal holds.
const CREATE_EVENT_DELETE_TRIGGER: &str =
    "CREATE TRIGGER IF NOT EXISTS events_refuse_delete BEFORE DELETE ON events BEGIN
  SELECT RAISE(ABORT, 'the ktask journal is append-only: an events row is never deleted');
END;";

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
///
/// [`Journal::put_state`] writes a row and [`Journal::get_state`],
/// [`Journal::all_states`] read them back. Nothing else in this crate touches the
/// table, and clearing it is ordinary work: what a run did is in `events`, and a
/// projection can always be built again from there (ADR-0023).
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
    CREATE_EVENT_UPDATE_TRIGGER,
    CREATE_EVENT_DELETE_TRIGGER,
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

    /// Every event the journal holds, oldest first.
    ///
    /// The whole journal, in the order it numbered its records — not the order of
    /// the instants, which two events stamped in the same nanosecond cannot be
    /// told apart by, and not the order the rows were written, which is a fact
    /// about whoever wrote them rather than about the run.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when `events` cannot be read; [`Error::Corrupt`] when a
    /// row describes no event: a number that cannot be a sequence, text that is not
    /// an instant, a task number no queue holds, or a payload that is not the entry
    /// its own column names. A read that refuses stops at the row it could not
    /// decode rather than handing back a journal with a hole in it, because a caller
    /// reading in order to replay would replay less of the run than it was given.
    pub fn events(&self) -> Result<Vec<Event>> {
        self.read_events(
            "SELECT seq, ts, task_id, kind, payload FROM events ORDER BY seq",
            params![],
        )
    }

    /// One task's events, oldest first.
    ///
    /// The records whose `task_id` names `task`, and nothing else. A queue-level
    /// event belongs to no task, so it is in [`Journal::events`] and in no per-task
    /// read; a caller that wants both reads both. The filter is the SQL's, which is
    /// what lets the read use `idx_events_task` — the index over `(task_id, seq)`
    /// `docs/DESIGN.md` asks for — instead of taking the whole journal to pick one
    /// task out of it.
    ///
    /// # Errors
    ///
    /// As [`Journal::events`]. A task holding no events is an empty `Vec` and not
    /// [`Error::NotFound`]: the queue numbers 4,294,967,295 positions and nearly all
    /// of them are empty at any moment, so asking about one is a question the journal
    /// answers with nothing.
    pub fn events_for(&self, task: TaskId) -> Result<Vec<Event>> {
        self.read_events(
            "SELECT seq, ts, task_id, kind, payload FROM events WHERE task_id = ?1 \
             ORDER BY seq",
            params![i64::from(task.get())],
        )
    }

    /// Every event ahead of a cursor, oldest first.
    ///
    /// `seq` is the newest sequence the caller already holds, so the answer is
    /// strictly after it and the record the cursor names is not handed back again.
    /// That is what makes this a cursor rather than a filter: a reader that polls it
    /// — the TUI does, so a run started in another process becomes visible — would
    /// otherwise be shown every event once per poll.
    ///
    /// The cursor need not name a record the journal still holds. A sequence spent by
    /// an interrupted commit is still a number a reader can be ahead of, and only the
    /// number is asked about.
    ///
    /// # Errors
    ///
    /// As [`Journal::events`]. A cursor beyond every sequence the column can hold is
    /// an empty read, because nothing can be ahead of it.
    pub fn events_since(&self, seq: EventSeq) -> Result<Vec<Event>> {
        // `seq` is a signed `INTEGER`, so a cursor wider than that is beyond every
        // row the table can hold: the widest number the column can compare against
        // gives the same empty answer, without a conversion this build would have to
        // explain away.
        let cursor = i64::try_from(seq.get()).unwrap_or(i64::MAX);
        self.read_events(
            "SELECT seq, ts, task_id, kind, payload FROM events WHERE seq > ?1 ORDER BY seq",
            params![cursor],
        )
    }

    /// Every event ahead of a cursor, handed to a callback one record at a time.
    ///
    /// The same read as [`Journal::events_since`] — the same cursor, the same
    /// order, the same rows — with one difference: it never holds them. A record
    /// arrives as the prepared statement steps onto it and is dropped as soon as
    /// `f` has taken it, so a journal of ten thousand events costs a reader the
    /// memory of one event and a journal of ten million costs the same. That is
    /// what lets a caller follow a run whose journal has outgrown the process
    /// reading it: the history and logs screens of VISION.md section 13, and the
    /// poll of section 12, are readers of a whole journal, not of a screenful.
    ///
    /// `from` means what it means to [`Journal::events_since`] — the newest
    /// sequence the caller already holds, exclusive, and a cursor need not name a
    /// record the journal still holds (ADR-0018). A cursor past the newest record
    /// streams nothing and is not an error.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when `events` cannot be read; [`Error::Corrupt`] when a
    /// row describes no event, refused exactly as the collected reads refuse it
    /// and for the same reason — a stream with a record quietly skipped is a
    /// replay that missed it. The error `f` returns is handed straight back,
    /// unchanged, and the read stops there: a reader that took ten thousand
    /// records and refused the next has read ten thousand, and the journal is left
    /// precisely as the read found it, with no row written and no sequence spent.
    pub fn for_each_event(
        &self,
        from: EventSeq,
        f: &mut dyn FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        // Clamped as `events_since` clamps it, because the two cursors mean the
        // same thing: a sequence wider than the signed `INTEGER` the column holds
        // is beyond every row the table can store, and the answer is the empty
        // stream rather than a conversion this build would have to explain away.
        let cursor = i64::try_from(from.get()).unwrap_or(i64::MAX);
        self.for_each_read(
            "SELECT seq, ts, task_id, kind, payload FROM events WHERE seq > ?1 ORDER BY seq",
            params![cursor],
            f,
        )
    }

    /// Run one of the collected read statements and hand back every row it returned.
    ///
    /// The statement comes from the caller because the three collected reads differ
    /// only in their `WHERE` clause, and the rule that turns a row into an [`Event`]
    /// is [`Journal::for_each_read`]'s rather than a copy of it kept here — a second
    /// decode step would be a rule that could drift from the one the streamed read
    /// uses. The `Vec` belongs to this function and not to that one because these
    /// three callers asked for every record at once; a caller that wants them one at
    /// a time calls [`Journal::for_each_event`] and never builds this.
    fn read_events(&self, sql: &str, query: &[&dyn ToSql]) -> Result<Vec<Event>> {
        let mut collected = Vec::new();
        self.for_each_read(sql, query, &mut |event| {
            collected.push(event);
            Ok(())
        })?;
        Ok(collected)
    }

    /// The one statement runner behind every read in this file.
    ///
    /// It prepares the caller's statement, steps it, and hands each decoded row to
    /// `f` — one row alive at a time, owned by the callback only for as long as the
    /// callback takes it, and never gathered here. Four reads share that much and
    /// differ only in the statement they hand in, so the row shape and the rule that
    /// turns a row into an [`Event`] are stated once: a second copy of the decode
    /// step would be a rule that could drift from the first, and a reader would
    /// learn about it from a replay rather than from a refusal.
    ///
    /// `f`'s error ends the read. It is returned as the read's own error, which is
    /// what lets a caller walk out of a journal it has read enough of (ADR-0020).
    fn for_each_read(
        &self,
        sql: &str,
        query: &[&dyn ToSql],
        f: &mut dyn FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query(query)?;
        while let Some(row) = rows.next()? {
            let event = decode_event(event_row(row)?)?;
            f(event)?;
        }
        Ok(())
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

/// One `events` row, in the types the schema declares its columns with.
///
/// The row is kept apart from the [`Event`] it describes because the five columns
/// are the durable thing and the envelope is what this build makes of them. The step
/// from one to the other is where a file that has stopped agreeing with its own
/// schema gets reported, and it can only do that while the two are distinguishable.
struct EventRow {
    seq: i64,
    ts: String,
    task_id: Option<i64>,
    kind: String,
    payload: String,
}

/// Map one result row onto [`EventRow`], in the order the read statements spell
/// their columns.
///
/// Only the widths are decided here: an `INTEGER` is read as the `i64` SQLite gives
/// it. What those numbers *mean* is [`decode_event`]'s decision, which keeps the
/// damage report away from the place that cannot say what went wrong with a column
/// rather than with a row.
fn event_row(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        seq: row.get(0)?,
        ts: row.get(1)?,
        task_id: row.get(2)?,
        kind: row.get(3)?,
        payload: row.get(4)?,
    })
}

/// The [`Event`] a stored row describes, or the damage report for a row that
/// describes none.
///
/// Four columns, four questions, and a row has to answer every one of them to be
/// read: is the number a sequence ([`event_sequence`]), is the text an instant
/// ([`stored_instant`]), is the number a queue position ([`stored_task`]), is the
/// payload the entry its own column names ([`stored_entry`])? A record this build
/// cannot read is a record it would otherwise invent, so every refusal is
/// [`Error::Corrupt`] carrying the sequence the reader got far enough to know.
fn decode_event(record: EventRow) -> Result<Event> {
    let EventRow {
        seq: written,
        ts,
        task_id,
        kind,
        payload,
    } = record;
    let seq = event_sequence(written)?;
    Ok(Event {
        seq,
        ts: stored_instant(&ts, seq)?,
        task_id: stored_task(task_id, seq)?,
        kind: stored_entry(&payload, &kind, seq)?,
    })
}

/// The instant the `ts` column documents, or the damage report for text that is not
/// it.
///
/// [`stamp_text`] writes that column as RFC 3339 in UTC and nothing else, and
/// `docs/DESIGN.md` Conventions fixes that as its one spelling. Text which is not it
/// did not come from this build — a hand-edited journal, or bytes that moved — and
/// guessing at a second format would be a read inventing an instant for the one
/// field whose whole purpose is to carry a real one.
fn stored_instant(text: &str, seq: EventSeq) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(text, &Rfc3339).map_err(|unparsable| Error::Corrupt {
        detail: format!(
            "the record at seq {seq} holds `{text}` in `ts`, which is not the RFC 3339 UTC instant \
             the column documents: {unparsable}",
        ),
        seq: Some(seq.get()),
    })
}

/// The queue position the `task_id` column names, or the damage report for a number
/// that is not one.
///
/// `None` is the schema's own answer — a queue-level event, the `NULL`
/// `docs/DESIGN.md` documents — and it is the only absence that is not damage. A
/// number outside a [`TaskId`] is: the column is a wider `INTEGER` than the
/// identifier because it has to hold that `NULL` as well, not because a queue can
/// hold more positions than this build can name.
fn stored_task(stored: Option<i64>, seq: EventSeq) -> Result<Option<TaskId>> {
    let Some(number) = stored else {
        return Ok(None);
    };
    u32::try_from(number)
        .map(|position| Some(TaskId::new(position)))
        .map_err(|out_of_range| Error::Corrupt {
            detail: format!(
                "the record at seq {seq} names {number} as its task, which is not a queue \
                 position: {out_of_range}",
            ),
            seq: Some(seq.get()),
        })
}

/// The catalog entry the payload holds, checked against the `kind` column meant to
/// name it, or the damage report for a row whose halves disagree.
///
/// Two columns carrying one string is the risk [`EventKind::discriminant`] exists to
/// keep honest: the append writes the same word to both, and a row naming one entry
/// in its column and another inside its payload cannot be answered with either — a
/// `WHERE kind = …` and a decoded enum would disagree about one record, which is the
/// exact failure the two-column layout buys the right to have. So a disagreement is
/// refused rather than settled in favour of one half.
fn stored_entry(payload: &str, column: &str, seq: EventSeq) -> Result<EventKind> {
    let decoded: EventKind =
        serde_json::from_str(payload).map_err(|undecodable| Error::Corrupt {
            detail: format!(
                "the record at seq {seq} holds a payload that is not a catalog entry: \
                 {undecodable}",
            ),
            seq: Some(seq.get()),
        })?;
    let named = decoded.discriminant();
    if named == column {
        return Ok(decoded);
    }
    Err(Error::Corrupt {
        detail: format!(
            "the record at seq {seq} names `{column}` in its `kind` column and `{named}` inside \
             its payload, so the row disagrees with itself about what happened",
        ),
        seq: Some(seq.get()),
    })
}

/// The queue: the `tasks` rows a plan document becomes, and the tasks they hold.
///
/// A plan file is an *input format* (`docs/DESIGN.md` Database schema), so the
/// database is the only place the queue itself lives. That is what makes "no
/// task is rewritten in place" true of a queue as well as of a journal:
/// [`Journal::put_tasks`] writes a plan's rows once, into a queue that holds
/// nothing, and [`Journal::tasks`] reads them back by id. There is no statement
/// in this crate that updates a row of `tasks`, and none that writes a status.
///
/// # What a row holds, and what it refuses to hold
///
/// A row holds what was *asked*: the four required sections in the columns
/// `docs/DESIGN.md` gives them, the block as authored, and the instant the
/// import ran. Three facts about a task are deliberately not in it.
///
/// - **Status** is the `TaskState` the journal implies, materialized in
///   `task_state` as a projection of it. A status column would be a second home
///   for one fact, so [`Journal::put_tasks`] does not store the status a caller
///   hands it, and [`Journal::tasks`] reports [`crate::TaskStatus::Pending`] — the
///   status a task has while nothing has been concluded about it. When the
///   projection lands, it replaces that constant here and nowhere else.
/// - **Title** is written, because a queue listing should not have to parse a
///   body to name its rows, and is never read back: [`crate::Task::title`] is a
///   projection of the body (ADR-0006), and a stored copy of it that disagreed
///   with its body would be two facts where the design has one.
/// - **Gate** is read out of the body on the way back (`task::gate_of`), because
///   a gate is marked by a section of the body rather than by a column or a
///   status, and the schema gives a row no gate to hold (ADR-0019).
///
/// # Why a module, and why this one
///
/// The row codec shares a connection and a schema with the event half of this
/// file and nothing else. The `impl` below is on [`Journal`] all the same, so
/// the queue is reached through the journal that owns the file rather than
/// through a second door a caller could hold open beside it.
mod tasks {
    use rusqlite::{Connection, Row, params};

    use super::{clock_nanos, stamp_text};
    use crate::task::{gate_of, task_id, validate};
    use crate::{Error, Journal, Result, Task, TaskId, TaskStatus};

    /// One queue row, in the order [`Journal::tasks`] spells its columns.
    ///
    /// `title`, `protocol` and `added_at` are absent on purpose: the first is a
    /// projection of `body` that no read should trust a second copy of, the
    /// second is NULL for every row this build writes, and the third dates a row
    /// for whoever asks when it arrived. None of the three is a field of [`Task`].
    struct TaskRow {
        id: i64,
        outcome: String,
        done_when: String,
        verify: String,
        refs: String,
        body: String,
    }

    impl Journal {
        /// Write a parsed plan as the queue, in document order.
        ///
        /// The queue is the plan, once. Each task keeps its position in the
        /// document as its id, so "task 3" means "the third block of the plan"
        /// in this file for as long as the queue stands — which is why a task
        /// handed over under some other id is refused rather than renumbered:
        /// the position is the fact, and a row free to choose its own number is
        /// a queue that cannot be quoted.
        ///
        /// Importing into a queue that already holds a task is refused, whatever
        /// the second plan contains. It is not merged, and nothing already there
        /// is edited in place: two plans claiming the same queue differ in order
        /// and in count, and deciding which one wins is an operator's call, not
        /// a supervisor's (VISION.md section 3, invariant 8). The refusal names how many tasks are
        /// in the way, because that is the fact the reader has to act on.
        ///
        /// One call is one transaction and one instant. Every row is stamped
        /// with the clock read inside the call — a value no caller hands in and
        /// no caller can move, for the same reason [`Journal::append`] stamps its
        /// own — so the rows of a plan are provably one import. A refusal at any
        /// task takes back the tasks before it, because half a plan is not a
        /// queue: its ids would be the positions of a document whose earlier
        /// half is missing.
        ///
        /// Each task is asked [`validate`] before its row is written, so the
        /// door to the queue and `plan lint` hold a task to one standard and a
        /// malformed task never enters (docs/CONTRACT.md).
        ///
        /// # Errors
        ///
        /// [`Error::Policy`] when the queue already holds a task (the refusal
        /// names how many), when a task is not the id its position gives it, or
        /// when a task lacks a required section; [`Error::Corrupt`] when the plan
        /// is longer than a queue position can number; [`Error::Database`] when a
        /// row cannot be written; [`Error::Serde`] when the clock reads an instant
        /// with no RFC 3339 spelling. In every case the queue holds exactly what
        /// it held before the call.
        pub fn put_tasks(&mut self, tasks: &[Task]) -> Result<()> {
            let added_at = stamp_text(clock_nanos())?;
            let transaction = self.conn.transaction()?;
            let existing = queue_length(&transaction)?;
            if existing > 0 {
                return Err(Error::Policy {
                    detail: format!(
                        "a plan is imported once, into an empty queue, and is never merged \
                         into one or edited in place; this queue is not empty (it holds \
                         {existing}) and its rows are left exactly as they are",
                    ),
                    paths: Vec::new(),
                });
            }
            for (index, task) in tasks.iter().enumerate() {
                let position = task_id(index)?;
                if task.id != position {
                    return Err(Error::Policy {
                        detail: format!(
                            "the task at plan position {position} was handed over as task {}; \
                             a queue's ids are its positions, numbered from one in document \
                             order",
                            task.id,
                        ),
                        paths: Vec::new(),
                    });
                }
                validate(task)?;
                transaction.execute(
                    "INSERT INTO tasks (id, title, outcome, done_when, verify, refs, protocol, \
                     body, added_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)",
                    params![
                        i64::from(position.get()),
                        task.title(),
                        task.outcome,
                        task.done_when,
                        task.verify,
                        task.refs,
                        task.body,
                        added_at
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        }

        /// The queue, read back by id.
        ///
        /// Ordered by id, which *is* queue order: the order the plan was
        /// written, not the order the rows happened to arrive and not the titles
        /// a reader sees. The order is the SQL's, as it is for every read in
        /// this file, so no caller has to remember to sort what it was handed.
        ///
        /// Status is reported as [`TaskStatus::Pending`] because there is no
        /// stored status to report: status is the `TaskState` the journal
        /// implies, and this is not that projection. A queue that holds nothing
        /// is an empty `Vec` rather than [`Error::NotFound`] — every project
        /// starts with one, and reading it is the first thing a run does.
        ///
        /// # Errors
        ///
        /// [`Error::Database`] when `tasks` cannot be read or a column is not the
        /// text or number its own schema declares; [`Error::Corrupt`] when a row
        /// is numbered outside the range a queue position can name, which is a
        /// row this build cannot say a word about.
        pub fn tasks(&self) -> Result<Vec<Task>> {
            let mut statement = self.conn.prepare(
                "SELECT id, outcome, done_when, verify, refs, body FROM tasks ORDER BY id",
            )?;
            statement
                .query_map([], task_row)?
                .map(|row| stored_task(row?))
                .collect()
        }
    }

    /// How many tasks the queue holds, which is the whole of the import rule.
    ///
    /// Asked inside the import's own transaction, so the count and the rows it
    /// protects are one snapshot rather than two facts with a gap between them.
    /// Measured on this toolchain: a second writer that commits after this read
    /// leaves the import's first `INSERT` failing with `SQLITE_BUSY_SNAPSHOT`
    /// (extended code 517), which surfaces as [`Error::Database`] and whose
    /// rollback this crate already relies on. A queue is therefore never merged
    /// into from two directions at once — either this import found the queue
    /// empty and owns it, or it fails loudly having written nothing.
    fn queue_length(conn: &Connection) -> Result<i64> {
        conn.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Map one result row onto [`TaskRow`], in the order the read spells them.
    ///
    /// Only the widths are decided here — an `INTEGER` is read as the `i64`
    /// SQLite hands back. What they mean is [`stored_task`], which is where a
    /// row that has stopped agreeing with its own schema gets reported.
    fn task_row(row: &Row<'_>) -> rusqlite::Result<TaskRow> {
        Ok(TaskRow {
            id: row.get(0)?,
            outcome: row.get(1)?,
            done_when: row.get(2)?,
            verify: row.get(3)?,
            refs: row.get(4)?,
            body: row.get(5)?,
        })
    }

    /// The [`Task`] a queue row holds.
    ///
    /// Two of its fields are supplied rather than read, because a row holds
    /// neither: `status`, which is the journal's to report, and `gate`, which
    /// only the body carries. Reading a stored copy of either would hand back a
    /// fact the text and the journal are free to disagree with.
    fn stored_task(record: TaskRow) -> Result<Task> {
        let id = stored_position(record.id)?;
        let gate = gate_of(&record.body);
        Ok(Task {
            id,
            status: TaskStatus::Pending,
            body: record.body,
            outcome: record.outcome,
            done_when: record.done_when,
            verify: record.verify,
            refs: record.refs,
            gate,
        })
    }

    /// The queue position a row's id names, or the damage report for a number
    /// that names none.
    ///
    /// A row like this is beyond what [`Journal::put_tasks`] writes, and a queue
    /// that answered with a truncated number would be quoting a task that does
    /// not exist.
    fn stored_position(stored: i64) -> Result<TaskId> {
        u32::try_from(stored)
            .map(TaskId::new)
            .map_err(|_| Error::Corrupt {
                detail: format!(
                    "a queue row is numbered {stored}, which is no task's position: ids run from \
                 one to {}",
                    u32::MAX,
                ),
                seq: None,
            })
    }

    #[cfg(test)]
    mod tests {
        use crate::journal::{Journal, journal_path};
        use crate::{Error, Task, TaskId, TaskStatus, parse_plan};
        use rusqlite::{Connection, params};
        use std::path::Path;
        use std::time::SystemTime;
        use tempfile::{TempDir, tempdir};
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;

        /// A plan document an operator might actually have written, chosen for
        /// what a row has to survive: a fenced block whose markup stays inert,
        /// a human gate, trailing whitespace kept in the body, and a heading
        /// long enough for the display cut to fall inside it.
        const PLAN: &str = "\
## Store the queue in the database

**Outcome:** the queue lives in SQLite, so nothing is rewritten in place.
**Done-when:** a plan round-trips through the queue unchanged.
**Verify:** `cargo nextest run -p ktask-core`
**Refs:** docs/DESIGN.md Database schema   

```markdown
## A heading inside a fence opens no task
**Gate:** a label inside a fence marks no gate either
```

## Approve the retention window

**Outcome:** an operator decides how long a journal is kept.
**Done-when:** the decision is recorded where a reader will find it.
**Verify:** `scripts/quality.sh doc`
**Refs:** VISION.md §11
**Gate:** a person approves the window, not the supervisor.

## T019 Store the queue in the database so that nothing about a task is rewritten ✓ in place

**Outcome:** a queue that lives in a file is a queue that can be edited by hand.
**Done-when:** the file is gone and the rows are the queue.
**Verify:** `cargo nextest run -p ktask-core`
**Refs:** docs/DESIGN.md Database schema
";

        /// The instant a staged row is dated at; which one is nobody's business.
        const AN_INSTANT: &str = "2026-09-17T12:00:00+00:00";

        /// A scratch parent for a journal: `docs/DESIGN.md` Conventions forbids
        /// a test from writing inside the repository.
        fn scratch() -> TempDir {
            tempdir().expect("a scratch directory below the system temp directory")
        }

        /// An opened journal in a scratch state directory, which is what a
        /// caller that has just registered finds on its first run.
        fn open_journal(state_dir: &Path) -> Journal {
            Journal::open(&journal_path(state_dir))
                .expect("a journal opens where its state directory is")
        }

        /// The scratch plan, parsed. Every round-trip assertion is against this.
        fn plan() -> Vec<Task> {
            parse_plan(PLAN).expect("the scratch plan is a plan the parser accepts")
        }

        /// One queue row, in the types the schema declares its columns with.
        #[derive(Debug, PartialEq)]
        struct Stored {
            id: i64,
            title: String,
            outcome: String,
            done_when: String,
            verify: String,
            refs: String,
            protocol: Option<String>,
            body: String,
            added_at: String,
        }

        /// Every queue row, in id order. The whole row, because the columns the
        /// read does not hand back can only be asserted against the table.
        fn stored_rows(conn: &Connection) -> Vec<Stored> {
            let mut statement = conn
                .prepare(
                    "SELECT id, title, outcome, done_when, verify, refs, protocol, body, \
                     added_at FROM tasks ORDER BY id",
                )
                .expect("tasks is always readable");
            statement
                .query_map([], |row| {
                    Ok(Stored {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        outcome: row.get(2)?,
                        done_when: row.get(3)?,
                        verify: row.get(4)?,
                        refs: row.get(5)?,
                        protocol: row.get(6)?,
                        body: row.get(7)?,
                        added_at: row.get(8)?,
                    })
                })
                .expect("tasks is readable")
                .collect::<rusqlite::Result<Vec<Stored>>>()
                .expect("every column reads as the type the schema declares it")
        }

        /// Write one queue row directly.
        ///
        /// The only way to reach a queue whose rows were not written in id
        /// order, or whose id is not a queue position at all — states an import
        /// cannot produce and a read still has to answer.
        fn stage_row(conn: &Connection, id: i64, title: &str, body: &str, added_at: &str) {
            conn.execute(
                "INSERT INTO tasks (id, title, outcome, done_when, verify, refs, protocol, \
                 body, added_at) VALUES (?1, ?2, 'an outcome', 'a done-when', 'a verify', \
                 'a ref', NULL, ?3, ?4)",
                params![id, title, body, added_at],
            )
            .expect("a staged row is a valid row");
        }

        #[test]
        fn a_plan_round_trips_through_the_queue_unchanged_and_in_document_order() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let parsed = plan();

            journal
                .put_tasks(&parsed)
                .expect("an empty queue takes a plan the parser accepted");

            assert_eq!(
                journal.tasks().expect("the queue is readable"),
                parsed,
                "every field of every task comes back as it was parsed — body byte for byte, \
                 gate and all — in the order the document wrote it"
            );
            assert_eq!(
                journal
                    .tasks()
                    .expect("the queue is readable")
                    .iter()
                    .map(|task| task.id.get())
                    .collect::<Vec<u32>>(),
                [1, 2, 3],
                "a queue's ids are its positions, so the third block of the document is task \
                 3 and not the row that happened to be written third"
            );
        }

        #[test]
        fn an_import_stamps_its_own_instant_projects_the_title_and_leaves_protocol_null() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let parsed = plan();

            let before = OffsetDateTime::from(SystemTime::now());
            journal
                .put_tasks(&parsed)
                .expect("an empty queue takes a plan the parser accepted");
            let after = OffsetDateTime::from(SystemTime::now());

            let rows = stored_rows(&journal.conn);
            let stamped = OffsetDateTime::parse(&rows[0].added_at, &Rfc3339).expect(
                "`added_at` holds RFC 3339 text, which is what the column's comment promises",
            );
            assert!(
                stamped >= before && stamped <= after,
                "the stamp is the clock read inside the call, not a constant and not a \
                 caller's guess: {stamped} falls outside {before}..={after}"
            );
            assert!(
                rows.iter().all(|row| row.added_at == rows[0].added_at),
                "one import is one instant: a queue whose rows arrived at three different \
                 times was not imported once: {rows:?}"
            );
            assert!(
                rows.iter().all(|row| row.protocol.is_none()),
                "`protocol` stays NULL for a task that names none, which is how the \
                 configured default stays the answer: {rows:?}"
            );
            assert_eq!(
                rows.iter()
                    .map(|row| (row.id, row.outcome.as_str(), row.verify.as_str()))
                    .collect::<Vec<_>>(),
                parsed
                    .iter()
                    .map(|task| (
                        i64::from(task.id.get()),
                        task.outcome.as_str(),
                        task.verify.as_str()
                    ))
                    .collect::<Vec<_>>(),
                "the four required sections are stored in the columns the design gives them, \
                 not stacked into one"
            );
            assert_eq!(
                rows.iter()
                    .map(|row| row.title.clone())
                    .collect::<Vec<String>>(),
                parsed
                    .iter()
                    .map(|task| task.title().to_owned())
                    .collect::<Vec<String>>(),
                "`title` is the body's projection, written for a reader who lists the queue \
                 without parsing a body — the third one cut at 80 characters like every other \
                 title in the queue"
            );
        }

        #[test]
        fn the_queue_holds_no_status_so_a_read_reports_the_task_unrun() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let mut task = plan().remove(0);
            task.status = TaskStatus::Failed;

            journal
                .put_tasks(std::slice::from_ref(&task))
                .expect("a complete task is queued whatever its caller claims about its status");

            assert_eq!(
                journal.tasks().expect("the queue is readable")[0].status,
                TaskStatus::Pending,
                "status has exactly one home — the `TaskState` the journal implies — so a \
                 status handed to the queue is neither stored nor handed back"
            );
        }

        #[test]
        fn importing_into_a_queue_that_holds_one_task_is_refused_naming_that_count() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            journal
                .put_tasks(&plan()[..1])
                .expect("an empty queue takes a plan");

            let error = journal
                .put_tasks(&plan()[1..])
                .expect_err("a plan is imported once, and this queue is already spoken for");
            let reported = error.to_string();

            assert!(
                matches!(error, Error::Policy { .. }),
                "a rule of the queue was broken, which is what `Policy` is for: {error}"
            );
            assert!(
                reported.contains("holds 1"),
                "the refusal names how many tasks are already in the way, rather than only \
                 that something is: {reported}"
            );
        }

        #[test]
        fn a_refused_import_leaves_the_rows_it_held_and_writes_nothing() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let held: Vec<Task> = plan().into_iter().take(2).collect();
            journal
                .put_tasks(&held)
                .expect("an empty queue takes a plan");

            let refused = journal
                .put_tasks(&plan()[2..])
                .expect_err("two tasks queued is not an empty queue");

            assert!(
                matches!(refused, Error::Policy { .. }),
                "the refusal is the queue's rule, not a database complaint: {refused}"
            );
            assert_eq!(
                journal.tasks().expect("the queue is readable"),
                held,
                "a refused import neither merges into the queue nor edits it in place: the \
                 queue still holds exactly what it held"
            );
            assert_eq!(
                stored_rows(&journal.conn).len(),
                2,
                "and the refusal wrote no row on its way to saying no"
            );
        }

        #[test]
        fn a_task_missing_a_required_section_stops_the_import_before_any_row_is_written() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let mut tasks = plan();
            tasks[1].verify = String::new();

            let error = journal
                .put_tasks(&tasks)
                .expect_err("a task with no Verify: command proves nothing, so it is not queued");

            assert!(
                matches!(error, Error::Policy { .. }),
                "the import asks the predicate `plan lint` asks, and reports it the same way: \
                 {error}"
            );
            assert!(
                error.to_string().contains("`Verify:`"),
                "the refusal names the section that is missing: {error}"
            );
            assert!(
                journal.tasks().expect("the queue is readable").is_empty(),
                "the plan was refused part way through, and its first task was not left \
                 behind: a queue is imported whole or not at all"
            );
        }

        #[test]
        fn a_task_handed_over_out_of_its_queue_position_is_refused_and_writes_nothing() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let mut tasks = plan();
            tasks[1].id = TaskId::new(9);

            let error = journal
                .put_tasks(&tasks)
                .expect_err("a plan whose second task calls itself task 9 is not a queue");

            assert!(
                matches!(error, Error::Policy { .. }),
                "a queue's ids being its positions is a rule, and rules refuse with \
                 `Policy`: {error}"
            );
            assert!(
                error.to_string().contains("task 9"),
                "the refusal names the id it refused to write: {error}"
            );
            assert!(
                journal.tasks().expect("the queue is readable").is_empty(),
                "the task before it was already inserted, and the refusal took that back"
            );
        }

        #[test]
        fn a_plan_holding_no_tasks_imports_nothing_and_leaves_the_queue_importable() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());

            journal.put_tasks(&[]).expect(
                "a document with no tasks in it imports nothing, which is nothing to write",
            );

            assert!(
                journal.tasks().expect("the queue is readable").is_empty(),
                "an empty plan leaves no row behind"
            );
            journal
                .put_tasks(&plan())
                .expect("an empty import did not fill the queue, so a real plan still fits");
            assert_eq!(
                journal.tasks().expect("the queue is readable").len(),
                3,
                "the queue counts tasks, not import attempts"
            );
        }

        #[test]
        fn the_queue_is_read_by_id_and_not_in_the_order_the_rows_were_written() {
            let parent = scratch();
            let journal = open_journal(parent.path());
            // Written 3, 1, 2, with titles and instants that both run the other
            // way to their ids, so only an ordering by id explains the answer.
            stage_row(
                &journal.conn,
                3,
                "Alpha",
                "third body",
                "2026-01-01T00:00:00+00:00",
            );
            stage_row(
                &journal.conn,
                1,
                "Zulu",
                "first body",
                "2026-01-03T00:00:00+00:00",
            );
            stage_row(
                &journal.conn,
                2,
                "Sierra",
                "second body",
                "2026-01-02T00:00:00+00:00",
            );

            let tasks = journal.tasks().expect("the queue is readable");

            assert_eq!(
                tasks
                    .iter()
                    .map(|task| (task.id.get(), task.body.as_str()))
                    .collect::<Vec<_>>(),
                [(1, "first body"), (2, "second body"), (3, "third body")],
                "the queue is ordered by its ids — not by the titles a reader sees, not by \
                 the instants rows were stamped, and not by the order they were written"
            );
            assert_eq!(
                tasks[0].title(),
                "first body",
                "the `title` column is never read back: a title is the body's projection, and \
                 a stored copy of it that disagreed with the body would be two facts"
            );
        }

        #[test]
        fn the_queue_survives_the_journal_being_closed_and_reopened() {
            let parent = scratch();
            let path = journal_path(parent.path());
            let parsed = plan();
            let mut journal = Journal::open(&path).expect("a journal opens where its directory is");
            journal
                .put_tasks(&parsed)
                .expect("an empty queue takes a plan the parser accepted");
            drop(journal);

            let reopened = Journal::open(&path).expect("the same file opens a second time");

            assert_eq!(
                reopened.tasks().expect("the reopened queue is readable"),
                parsed,
                "the queue lives in the file, so closing and reopening it changes nothing \
                 about what the queue holds"
            );
        }

        #[test]
        fn a_row_numbered_below_the_first_queue_position_is_read_as_damage() {
            let parent = scratch();
            let journal = open_journal(parent.path());
            stage_row(
                &journal.conn,
                -1,
                "A row outside the queue",
                "body",
                AN_INSTANT,
            );

            let error = journal
                .tasks()
                .expect_err("a row numbered -1 is not at a queue position");

            assert!(
                matches!(error, Error::Corrupt { .. }),
                "the row is in the file and cannot be trusted, which is what `Corrupt` is for: \
                 {error}"
            );
            assert!(
                error.to_string().contains("-1"),
                "the refusal quotes the number it refused: {error}"
            );
        }

        #[test]
        fn a_row_numbered_past_the_largest_queue_position_is_read_as_damage() {
            let parent = scratch();
            let journal = open_journal(parent.path());
            let widest = i64::from(u32::MAX) + 1;
            stage_row(
                &journal.conn,
                widest,
                "A row past the queue",
                "body",
                AN_INSTANT,
            );

            let error = journal
                .tasks()
                .expect_err("a queue position no task id can name is not a queue position");

            assert!(
                matches!(error, Error::Corrupt { .. }),
                "the number is out of a queue's range, which is damage and not an empty \
                 answer: {error}"
            );
            assert!(
                error.to_string().contains(&widest.to_string()),
                "the refusal quotes the number it refused: {error}"
            );
        }
    }
}

/// The materialized state: the `task_state` rows the journal implies.
///
/// `docs/DESIGN.md` Database schema calls `task_state` "the materialized current
/// state: a projection, rebuildable by replay", and that phrase decides
/// everything below. The table holds the answer to *where is each task now*,
/// which every screen, every `status` line and the supervisor's own next move ask
/// on every tick; `events` holds what makes that answer true. A projection may be
/// rewritten in place and may be dropped and built again, which is why this table
/// carries none of the append-only guards (ADR-0017): what a run did belongs to
/// the journal, and what it adds up to at this instant is a derived fact with no
/// history of its own.
///
/// # Who writes it, and why writing it is not recording
///
/// Two calls write this table and they share one write path. The recorder moves
/// one task with [`Journal::put_state`], once per transition [`crate::apply`]
/// accepted: the event first and the projection second, so the durable fact is in
/// the file before the summary that depends on it changes (VISION.md section 3,
/// invariant 2). [`Journal::rebuild_state`] writes the whole table from the
/// journal, which is what makes the table safe to be a summary at all. Neither
/// records anything: no `events` row is inserted and no sequence number is spent —
/// one transition is one event in the source of truth and one row here, and a
/// second event for the same decision would be two histories a replay had to
/// agree between.
///
/// # Why one row per task, stamped by the journal
///
/// A row is keyed by task, so writing a state twice overwrites it instead of
/// leaving a reader to choose between two answers ([`Journal::get_state`],
/// [`Journal::all_states`]). `updated_at` is stamped from the same clock
/// [`Journal::append`] reads rather than handed in by a caller, for the reason an
/// event's instant is: it says when *this file* learned the fact, and a caller
/// able to move it could move the projection's own past. It dates a row and orders
/// nothing — [`Journal::all_states`] goes by task id, because what is current is a
/// set rather than a sequence.
///
/// # Why a rebuild clears and refills in one transaction
///
/// Rebuilding is the recovery path, so it has to be safer than the work it
/// repairs. The fold is finished before the first row is touched, so a journal
/// that cannot be replayed is refused with the projection still standing, and the
/// clear-plus-rewrite runs as one transaction, so a write refused partway leaves
/// the projection the run left rather than an empty table and two rows. An absent
/// row reads as "this task has not started", which is the one misreading a
/// supervisor cannot recover from: it hands finished work to a provider again.
///
/// # Why a module, and why this one
///
/// The row codec shares a connection and a schema with the event half of this file
/// and nothing else, exactly as the queue module above does. The `impl` below is
/// on [`Journal`] all the same, so the projection is reached through the journal
/// that owns the file rather than through a second door a caller could hold open
/// beside it.
mod projection {
    use std::collections::BTreeMap;

    use rusqlite::{Connection, OptionalExtension as _, Row, params};

    use super::{clock_nanos, stamp_text};
    use crate::{Error, EventSeq, Journal, Result, TaskId, TaskState, apply};

    /// One projected row, in the types the schema declares its columns with.
    ///
    /// `updated_at` is absent on purpose: no read here hands it back, because the
    /// column dates a row for whoever opens the file and answers no question the
    /// state machine asks. Every write still fills it in.
    struct StateRow {
        task_id: i64,
        state_json: String,
    }

    impl Journal {
        /// Write the state one task is in, replacing whatever it had.
        ///
        /// The caller is the recorder, once per accepted transition: after the
        /// event for that transition is already in `events`, and after
        /// [`crate::apply`] has said the transition is legal. Nothing here decides
        /// whether a transition is allowed and nothing here consults the queue —
        /// the projection is written for the id it is handed, which is what lets a
        /// rebuild replay events whose task has long since left the queue.
        ///
        /// The write is one statement, so it is one transaction: an overwrite
        /// replaces the row rather than leaving a second one behind, and there is no
        /// moment — not even a crash partway through the write — in which the task
        /// has no state at all. Clearing the row and then inserting the new one has
        /// exactly such a moment, which is why this pairs an insert with the
        /// `ON CONFLICT` update of the row it collided with.
        ///
        /// `updated_at` is the clock read inside the call, in the same RFC 3339 UTC
        /// spelling as an event's `ts` (see [`Journal::append`]); no caller hands it
        /// in.
        ///
        /// # Errors
        ///
        /// [`Error::Serde`] when the state has no JSON encoding, or when the clock
        /// reads an instant with no RFC 3339 spelling; [`Error::Database`] when
        /// SQLite refuses the write. Either way the row the task held before the
        /// call is the row it holds after: a statement that failed wrote nothing.
        pub fn put_state(&mut self, task: TaskId, state: &TaskState) -> Result<()> {
            write_state(&self.conn, task, state)
        }

        /// The state one task is in, or `None` while nothing has concluded one.
        ///
        /// Absence is a real answer rather than damage: a task that has been queued
        /// and never run has no row, and reading the projection of a queue that has
        /// not started is the first thing every run does. It is not
        /// [`Error::NotFound`] either — the caller asked what a task's state is, and
        /// "nothing yet" is that answer. A task whose row cannot be decoded is
        /// damage, and is refused as [`Journal::all_states`] refuses it.
        ///
        /// # Errors
        ///
        /// [`Error::Database`] when `task_state` cannot be read or a column is not
        /// the type its schema declares; [`Error::Corrupt`] when the row's
        /// `state_json` is not a [`TaskState`], which is refused rather than read as
        /// absent — a projection that quietly answered `None` would start work a run
        /// had already finished.
        pub fn get_state(&self, task: TaskId) -> Result<Option<TaskState>> {
            let found = self
                .conn
                .query_row(
                    "SELECT task_id, state_json FROM task_state WHERE task_id = ?1",
                    params![i64::from(task.get())],
                    state_row,
                )
                .optional()?;
            let Some(record) = found else {
                return Ok(None);
            };
            let (_, state) = stored_state(record)?;
            Ok(Some(state))
        }

        /// Every task the projection holds a state for, in id order.
        ///
        /// Keyed by [`TaskId`], which is what makes the order part of the type: a
        /// `BTreeMap` iterates by id ascending, so queue order survives without a
        /// caller remembering to sort. The SQL says `ORDER BY task_id` all the same
        /// — the order is the read's, as it is for every read in this file, so a row
        /// arrives already known to be the one after the last.
        ///
        /// A task with no row is simply absent: the map is the projection, not the
        /// queue, and it is [`Journal::get_state`] that answers for one task.
        /// Ordering is by id and never by `updated_at`, because what is current is a
        /// set; a caller wanting the most recently moved task sorts what it was
        /// handed.
        ///
        /// # Errors
        ///
        /// As [`Journal::get_state`], plus [`Error::Corrupt`] when a row is numbered
        /// outside the ids a task can have. A read that refuses stops at the row it
        /// could not decode rather than handing back a projection with a task missing
        /// from the middle of it: a caller that read a short list would conclude work
        /// had not happened.
        pub fn all_states(&self) -> Result<BTreeMap<TaskId, TaskState>> {
            let mut statement = self
                .conn
                .prepare("SELECT task_id, state_json FROM task_state ORDER BY task_id")?;
            statement
                .query_map([], state_row)?
                .map(|row| stored_state(row?))
                .collect()
        }

        /// Drop the materialized state and build it again from the events alone.
        ///
        /// This is the operation that makes `task_state` a projection rather than a
        /// second source of truth (VISION.md section 5, `docs/DESIGN.md` Database
        /// schema): the table is cleared, every event the journal holds is folded
        /// through [`crate::apply`], and each task's state is written back. What a
        /// rebuild leaves is exactly what the run's own [`Journal::put_state`] calls
        /// left, so the projection can be thrown away after a crash instead of
        /// trusted — and a caller that suspects it drifted has a repair that does not
        /// need to know what drifted.
        ///
        /// Two orders make that equivalence, and both are the journal's rather than a
        /// caller's. The fold reads in `seq` order (see [`Journal::for_each_event`]),
        /// because a phase folded in before the attempt that entered it is a state no
        /// run was ever in. And the fold finishes before the first row is touched,
        /// with the clear and the rewrite as one transaction, so the projection is
        /// never observed mid-rebuild: it is the old one, or it is the new one. The
        /// fold keeps one state per task rather than one per event, so a journal of
        /// ten thousand records costs a rebuild the memory of the tasks it holds.
        ///
        /// Rebuilt rows are stamped with the instant the rebuild wrote them, not with
        /// the instant of the event behind them, which is what lets an operator tell a
        /// repaired projection from one a run wrote (ADR-0023).
        ///
        /// # Errors
        ///
        /// [`Error::Corrupt`] when the replay reaches an event the state machine
        /// refuses at the state it reaches, carrying the sequence of that record so
        /// the journal can be opened at the line that contradicts it; [`Error::Serde`]
        /// when a state has no JSON encoding, or the clock reads an instant with no
        /// RFC 3339 spelling; [`Error::Database`] when `events` cannot be read or the
        /// rewrite is refused. A refusal anywhere leaves the projection holding what
        /// it held before the call, because nothing is written until the whole journal
        /// has folded.
        pub fn rebuild_state(&mut self) -> Result<()> {
            let replayed = self.replayed_states()?;
            let transaction = self.conn.transaction()?;
            transaction.execute("DELETE FROM task_state", [])?;
            for (task, state) in &replayed {
                write_state(&transaction, *task, state)?;
            }
            transaction.commit()?;
            Ok(())
        }

        /// Fold every event the journal holds into the state each task is in.
        ///
        /// Every task starts at [`TaskState::Queued`] — the state a parsed plan lands
        /// in, and the state a [`crate::EventKind::TaskQueued`] record is the
        /// journal's record of — and is moved on by each of its own events in
        /// sequence order. An event whose `task_id` is `NULL` is about the queue: it
        /// is in the journal to be read and belongs to no accumulator.
        ///
        /// A record the machine refuses is the only failure this fold has, and it is
        /// reported rather than stopped at. A journal that reaches a state where its
        /// own next event is illegal claims something no run could have done, which is
        /// damage in the durable record; and the alternative — folding up to the
        /// refusal and keeping the answer — would hand a caller the projection of a
        /// run that quietly stopped, a lie about a task instead of an error about a
        /// file. The sequence is carried because "which record" is the question an
        /// operator can act on.
        fn replayed_states(&self) -> Result<BTreeMap<TaskId, TaskState>> {
            let mut folded: BTreeMap<TaskId, TaskState> = BTreeMap::new();
            self.for_each_event(EventSeq::new(0), &mut |event| {
                let Some(task) = event.task_id else {
                    return Ok(());
                };
                let held = folded.entry(task).or_insert(TaskState::Queued);
                let moved = apply(held, &event.kind).map_err(|refused| Error::Corrupt {
                    detail: format!(
                        "the journal does not replay: {refused}, and task {task} cannot be \
                         projected past it",
                    ),
                    seq: Some(event.seq.get()),
                })?;
                *held = moved;
                Ok(())
            })?;
            Ok(folded)
        }
    }

    /// Put one [`TaskState`] in one task's row, overwriting whatever was there.
    ///
    /// [`Journal::put_state`] hands this its own connection and
    /// [`Journal::rebuild_state`] hands it the transaction it has open — the only way
    /// a rebuild can write through the same statement a run writes through, since
    /// `rusqlite` borrows the connection for the life of a transaction and no method
    /// of [`Journal`] can be called inside one. Two callers writing a table this
    /// load-bearing is how a projection ends up disagreeing with itself, so the
    /// encoding, the stamp and the statement live here and both of them come through.
    ///
    /// Splitting the write out is also what keeps the equivalence a rebuild is judged
    /// on honest: the rows a replay leaves are the rows a run leaves, byte for byte,
    /// because there is no second writer to drift from the first.
    fn write_state(conn: &Connection, task: TaskId, state: &TaskState) -> Result<()> {
        let state_json = serde_json::to_string(state)?;
        let updated_at = stamp_text(clock_nanos())?;
        conn.execute(
            "INSERT INTO task_state (task_id, state_json, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(task_id) DO UPDATE SET state_json = excluded.state_json, \
             updated_at = excluded.updated_at",
            params![i64::from(task.get()), state_json, updated_at],
        )?;
        Ok(())
    }

    /// Map one result row onto [`StateRow`], in the order both reads spell them.
    ///
    /// The two reads ask for the same two columns in the same order so that one
    /// mapper and one decode step serve both: a second decoder would be a rule that
    /// could drift from the first, and then one read could call a row legible while
    /// the other called it damage.
    fn state_row(row: &Row<'_>) -> rusqlite::Result<StateRow> {
        Ok(StateRow {
            task_id: row.get(0)?,
            state_json: row.get(1)?,
        })
    }

    /// The [`TaskId`] and [`TaskState`] a projected row holds, or the damage report
    /// for a row that holds neither.
    ///
    /// Two columns, two questions, in the order `decode_event` asks them of an
    /// `events` row: is the number a task at all, and is the text a state? A row
    /// this build cannot read is a state it would otherwise invent, so every refusal
    /// is [`Error::Corrupt`], naming the task the row claimed.
    fn stored_state(record: StateRow) -> Result<(TaskId, TaskState)> {
        let StateRow {
            task_id,
            state_json,
        } = record;
        let task = projected_task(task_id)?;
        Ok((task, decode_state(&state_json, task)?))
    }

    /// The task a `task_id` names, or the damage report for a number that names
    /// none.
    ///
    /// The column is a signed `INTEGER`, wider than the `u32` a [`TaskId`] wraps, so
    /// a hand-edited file can hold a number no task has — and a projection that
    /// answered by truncating it would quote a task that does not exist while hiding
    /// the row that really was damaged.
    fn projected_task(stored: i64) -> Result<TaskId> {
        u32::try_from(stored)
            .map(TaskId::new)
            .map_err(|out_of_range| Error::Corrupt {
                detail: format!(
                    "`task_state` holds a row numbered {stored}, which is no task's \
                     position: {out_of_range}, and no id is wider than {}",
                    u32::MAX,
                ),
                seq: None,
            })
    }

    /// The [`TaskState`] a row's JSON holds, or the damage report for text that
    /// holds none.
    ///
    /// [`Journal::put_state`] writes `serde`'s encoding of a [`TaskState`] and
    /// nothing else, so text outside that grammar did not come from this build.
    /// Guessing at what it meant — reading an unknown variant as some state, or as no
    /// state — is a projection inventing a fact about a task nobody recorded.
    fn decode_state(payload: &str, task: TaskId) -> Result<TaskState> {
        serde_json::from_str(payload).map_err(|undecodable| Error::Corrupt {
            detail: format!(
                "the projected state of task {task} is not a `TaskState`: {undecodable}"
            ),
            seq: None,
        })
    }

    #[cfg(test)]
    mod tests {
        use crate::journal::{Journal, journal_path};
        use crate::{
            AttemptId, Error, EventKind, EventSeq, FailureClass, PauseReason, Phase, Stream,
            TaskId, TaskState, apply,
        };
        use rusqlite::{Connection, OptionalExtension as _, params};
        use std::path::Path;
        use std::time::Duration;
        use tempfile::{TempDir, tempdir};
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;
        use time::macros::datetime;

        /// A scratch parent for a journal: `docs/DESIGN.md` Conventions forbids a
        /// test from writing inside the repository.
        fn scratch() -> TempDir {
            tempdir().expect("a scratch directory below the system temp directory")
        }

        /// An opened journal in a scratch state directory.
        fn open_journal(state_dir: &Path) -> Journal {
            Journal::open(&journal_path(state_dir))
                .expect("a journal opens where its state directory is")
        }

        /// One state per `TaskState` variant, in the order the type declares them:
        /// the four that carry nothing, the ones that carry an attempt or a commit,
        /// the one that carries a person and an instant, the pause that carries a
        /// boxed state, and the ways a task ends without being done.
        ///
        /// Every variant, because [`Journal::put_state`] is variant-blind — it
        /// writes `serde`'s JSON of whatever it is handed — so the only thing that
        /// can make a state unreadable through the projection is the encoding, and
        /// an encoding nothing wrote is an encoding nothing read either. A state
        /// left out of this list is a state whose round trip nobody has seen.
        fn states() -> Vec<TaskState> {
            vec![
                TaskState::Queued,
                TaskState::Preflight,
                TaskState::Running {
                    attempt: AttemptId::new(2),
                    phase: Phase::Green,
                },
                TaskState::Remediating {
                    attempt: AttemptId::new(4),
                    phase: Phase::Red,
                },
                TaskState::Verifying {
                    attempt: AttemptId::new(1),
                },
                TaskState::Publishing {
                    attempt: AttemptId::new(2),
                },
                TaskState::PublishedVerified {
                    commit: "b42c45f".to_owned(),
                },
                TaskState::Done,
                TaskState::Acknowledged {
                    by: "operator".to_owned(),
                    at: datetime!(2026-09-17 12:34:56 UTC),
                },
                TaskState::Paused {
                    reason: PauseReason::Limit {
                        until: Some(datetime!(2026-09-17 13:00:00 UTC)),
                    },
                    resume_to: Box::new(TaskState::Running {
                        attempt: AttemptId::new(1),
                        phase: Phase::Red,
                    }),
                },
                TaskState::Failed {
                    class: FailureClass::ProviderLimit,
                    detail: "the provider advertised no reset".to_owned(),
                },
                TaskState::Cancelled,
            ]
        }

        /// The task a test writes `state` under: the states list, numbered from the
        /// first queue position.
        fn task_holding(states: &[TaskState], index: usize) -> TaskId {
            let position = u32::try_from(index + 1).expect("a scratch queue is short");
            assert!(index < states.len(), "a scratch state index is in the list");
            TaskId::new(position)
        }

        /// One projected row, as the table holds it.
        #[derive(Debug, PartialEq)]
        struct Stored {
            task_id: i64,
            state_json: String,
            updated_at: String,
        }

        /// Every projected row, in id order. The whole row, because `updated_at`
        /// and the JSON text are what the table holds and what no read hands back.
        fn stored_rows(conn: &Connection) -> Vec<Stored> {
            let mut statement = conn
                .prepare("SELECT task_id, state_json, updated_at FROM task_state ORDER BY task_id")
                .expect("task_state is always readable");
            statement
                .query_map([], |row| {
                    Ok(Stored {
                        task_id: row.get(0)?,
                        state_json: row.get(1)?,
                        updated_at: row.get(2)?,
                    })
                })
                .expect("task_state is readable")
                .collect::<rusqlite::Result<Vec<Stored>>>()
                .expect("every column reads as the type the schema declares it")
        }

        /// Write a projected row directly, dated as stated.
        ///
        /// The only way to reach a projection whose rows no write can produce — a
        /// number outside a task id, a `state_json` that is not a state — which is
        /// what a read's refusal has to be tested against. The instant is a
        /// parameter because the one thing a read's order can be confused with is
        /// the order the rows arrived in, and only a writer outside this module
        /// can date rows in an order that disagrees with both.
        fn stage_row(conn: &Connection, task_id: i64, state_json: &str, updated_at: &str) {
            conn.execute(
                "INSERT INTO task_state (task_id, state_json, updated_at) VALUES (?1, ?2, ?3)",
                params![task_id, state_json, updated_at],
            )
            .expect("a staged row is a row the schema accepts");
        }

        /// The JSON [`Journal::put_state`] is expected to have stored for `state`.
        fn stored_json(state: &TaskState) -> String {
            serde_json::to_string(state).expect("every state has a JSON encoding")
        }

        #[test]
        fn every_state_round_trips_through_the_projection_exactly_as_it_was_written() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let written = states();

            for (index, state) in written.iter().enumerate() {
                let task = task_holding(&written, index);
                journal
                    .put_state(task, state)
                    .expect("a fresh projection takes a state for any task");
                assert_eq!(
                    journal
                        .get_state(task)
                        .expect("the projection is readable")
                        .as_ref(),
                    Some(state),
                    "task {task} reads back the state it was written with, payload and all"
                );
                assert_eq!(
                    stored_rows(&journal.conn)[index].state_json,
                    stored_json(state),
                    "the row holds `serde`'s JSON of the state, which is what makes the column \
                     legible to anything that opens the file"
                );
            }
        }

        #[test]
        fn a_task_the_projection_was_never_given_a_state_for_reads_as_none() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            journal
                .put_state(TaskId::new(2), &TaskState::Queued)
                .expect("a fresh projection takes a state");

            assert_eq!(
                journal
                    .get_state(TaskId::new(1))
                    .expect("a read of an absent state is an answer, not a failure"),
                None,
                "a queued task that has never run has no state to report, and asking for one \
                 buys nothing rather than a `NotFound`"
            );
            assert_eq!(
                stored_rows(&journal.conn).len(),
                1,
                "reading a task the projection does not hold writes no row for it"
            );
        }

        #[test]
        fn writing_a_state_twice_overwrites_the_row_rather_than_adding_another_one() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let task = TaskId::new(7);
            let later = TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Green,
            };

            journal
                .put_state(task, &TaskState::Queued)
                .expect("the first write is an ordinary write");
            journal
                .put_state(task, &later)
                .expect("the second write is an ordinary write too");

            let rows = stored_rows(&journal.conn);
            assert_eq!(
                rows.len(),
                1,
                "one task is one row, however often its state was written: two rows would \
                 leave a reader choosing between two answers"
            );
            assert_eq!(
                rows[0].state_json,
                stored_json(&later),
                "the row holds the state last written and nothing of the one before it"
            );
            assert_eq!(
                journal.get_state(task).expect("the projection is readable"),
                Some(later),
                "the overwritten state is gone, not shadowed"
            );
        }

        #[test]
        fn an_overwrite_of_one_task_leaves_every_other_task_as_it_was() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let untouched = TaskState::Verifying {
                attempt: AttemptId::new(3),
            };
            journal
                .put_state(TaskId::new(1), &TaskState::Queued)
                .expect("a fresh projection takes a state");
            journal
                .put_state(TaskId::new(2), &untouched)
                .expect("a fresh projection takes a state");

            journal
                .put_state(TaskId::new(1), &TaskState::Done)
                .expect("overwriting task 1 is ordinary work");

            assert_eq!(
                journal
                    .get_state(TaskId::new(2))
                    .expect("the projection is readable"),
                Some(untouched),
                "the write was aimed at one task's row, and yet a state nobody handed in \
                 appears beside another task"
            );
        }

        #[test]
        fn the_journal_stamps_updated_at_and_the_next_write_refreshes_it() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let task = TaskId::new(1);

            journal
                .put_state(task, &TaskState::Queued)
                .expect("a fresh projection takes a state");
            let first = stored_rows(&journal.conn)[0].updated_at.clone();
            std::thread::sleep(Duration::from_millis(2));
            journal
                .put_state(task, &TaskState::Done)
                .expect("overwriting a state is ordinary work");
            let second = stored_rows(&journal.conn)[0].updated_at.clone();

            let written = OffsetDateTime::parse(&first, &Rfc3339)
                .expect("the stamp is the RFC 3339 UTC instant the column documents");
            let refreshed = OffsetDateTime::parse(&second, &Rfc3339)
                .expect("the refresh is stamped the same way");
            assert!(
                refreshed > written,
                "the second write is stamped with when it happened, not with the instant the \
                 first write used: {first} then {second}"
            );
        }

        #[test]
        fn all_states_hands_back_every_task_written_and_nobody_else() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let written = states();

            for (index, state) in written.iter().enumerate() {
                journal
                    .put_state(task_holding(&written, index), state)
                    .expect("a fresh projection takes a state");
            }

            let projection = journal.all_states().expect("the projection is readable");
            assert_eq!(
                projection.len(),
                written.len(),
                "one entry per task the projection holds, and none for a task that was \
                 never written"
            );
            let next_in_queue =
                TaskId::new(u32::try_from(written.len() + 1).expect("a scratch queue is short"));
            assert_eq!(
                projection.get(&next_in_queue),
                None,
                "the map is the projection, not the queue: the task the queue reaches next, \
                 which nobody has concluded, is absent from it"
            );
            for (index, state) in written.iter().enumerate() {
                let task = task_holding(&written, index);
                assert_eq!(
                    projection.get(&task),
                    Some(state),
                    "task {task} is paired with the state it was written with, so a read \
                     cannot mix the tasks up"
                );
            }
        }

        #[test]
        fn all_states_returns_the_tasks_in_id_order_whatever_order_they_were_written() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let widest = TaskId::new(u32::MAX);

            for task in [TaskId::new(11), widest, TaskId::new(2), TaskId::new(9)] {
                journal
                    .put_state(
                        task,
                        &TaskState::Running {
                            attempt: AttemptId::new(task.get()),
                            phase: Phase::Verify,
                        },
                    )
                    .expect("a fresh projection takes a state");
            }

            let projection = journal.all_states().expect("the projection is readable");
            assert_eq!(
                projection
                    .keys()
                    .copied()
                    .map(TaskId::get)
                    .collect::<Vec<u32>>(),
                [2, 9, 11, u32::MAX],
                "queue order, not the order the rows happened to arrive — and `updated_at` \
                 orders nothing"
            );
            assert_eq!(
                projection.get(&widest),
                Some(&TaskState::Running {
                    attempt: AttemptId::new(u32::MAX),
                    phase: Phase::Verify,
                }),
                "the widest id a task can have is a task like any other, and its state is \
                 its own rather than its neighbour's"
            );
        }

        #[test]
        fn the_projection_of_a_journal_that_holds_nothing_is_empty_rather_than_missing() {
            let parent = scratch();
            let journal = open_journal(parent.path());

            assert!(
                journal
                    .all_states()
                    .expect("an empty projection is a readable projection")
                    .is_empty(),
                "every project starts with nothing materialized, and reading it is a fact \
                 rather than an error"
            );
        }

        #[test]
        fn the_projection_survives_the_journal_being_closed_and_reopened() {
            let parent = scratch();
            let file = journal_path(parent.path());
            let written = TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Verifying {
                    attempt: AttemptId::new(1),
                }),
            };

            {
                let mut journal = Journal::open(&file).expect("a new journal");
                journal
                    .put_state(TaskId::new(3), &written)
                    .expect("a fresh projection takes a state");
            }

            let reopened = Journal::open(&file).expect("the journal reopens");
            assert_eq!(
                reopened
                    .get_state(TaskId::new(3))
                    .expect("the projection is readable"),
                Some(written),
                "a pause survived the process that made it, boxed state and all — which is \
                 what lets a resumed run resume that wait rather than guess one"
            );
        }

        #[test]
        fn a_state_write_records_no_event_and_spends_no_sequence() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());

            for task in [TaskId::new(1), TaskId::new(2)] {
                journal
                    .put_state(task, &TaskState::Done)
                    .expect("a fresh projection takes a state");
                journal
                    .put_state(task, &TaskState::Cancelled)
                    .expect("overwriting a state is ordinary work");
            }

            assert!(
                journal
                    .events()
                    .expect("the journal is readable")
                    .is_empty(),
                "a projection is derived from the journal, so writing it must not write the \
                 journal: two records of one decision are two histories"
            );
            let seq = journal
                .append(None, &EventKind::PreflightStarted)
                .expect("an append after a state write still works");
            assert_eq!(
                seq,
                EventSeq::new(1),
                "four state writes spent no sequence number, so the first event this journal \
                 holds is still seq 1"
            );
        }

        #[test]
        fn a_projected_row_whose_json_is_not_a_state_is_read_as_damage() {
            let parent = scratch();
            let journal = open_journal(parent.path());
            stage_row(
                &journal.conn,
                5,
                "not a state at all",
                "2026-09-17T12:00:00+00:00",
            );

            let one = journal
                .get_state(TaskId::new(5))
                .expect_err("a row that is not JSON is not a state either");
            let all = journal
                .all_states()
                .expect_err("the same row stops a whole-projection read");

            for refusal in [one, all] {
                assert!(
                    matches!(refusal, Error::Corrupt { .. }),
                    "a row this build cannot decode is damage, not an absent state: {refusal}"
                );
                assert!(
                    refusal.to_string().contains("task 5"),
                    "the report names the task whose state could not be read: {refusal}"
                );
            }
        }

        #[test]
        fn a_projected_row_holding_a_variant_this_build_does_not_name_is_damage() {
            let parent = scratch();
            let journal = open_journal(parent.path());
            stage_row(
                &journal.conn,
                1,
                r#"{"Nudging":{"attempt":1}}"#,
                "2026-09-17T12:00:00+00:00",
            );

            let refusal = journal
                .get_state(TaskId::new(1))
                .expect_err("no state of this build is named `Nudging`");

            assert!(
                matches!(refusal, Error::Corrupt { .. }),
                "an unknown variant is refused rather than read as some state, or as none: \
                 {refusal}"
            );
        }

        #[test]
        fn a_projected_row_numbered_past_a_task_id_is_read_as_damage() {
            let parent = scratch();
            let journal = open_journal(parent.path());
            let widest = i64::from(u32::MAX);
            stage_row(
                &journal.conn,
                widest + 1,
                r#""Done""#,
                "2026-09-17T12:00:00+00:00",
            );

            let refusal = journal
                .all_states()
                .expect_err("a row numbered past every task id is not a projection");

            assert!(
                matches!(refusal, Error::Corrupt { .. }),
                "the number is out of a task id's range, which is damage and not an empty \
                 answer: {refusal}"
            );
            assert!(
                refusal.to_string().contains(&(widest + 1).to_string()),
                "the refusal quotes the number it refused: {refusal}"
            );
        }

        #[test]
        fn the_whole_projection_is_read_from_the_lowest_id_rather_than_the_earliest_stamp() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            journal
                .put_state(TaskId::new(1), &TaskState::Queued)
                .expect("a fresh projection takes a state");
            // Dated so that the order of the stamps disagrees with the order of the
            // ids: only a read ordered by id meets task 2 first. The order the rows
            // arrived in cannot disagree — a `task_id` *is* the rowid (ADR-0023) — so
            // the stamps are the one order a test can separate it from.
            stage_row(&journal.conn, 4, "not a state", "2020-01-01T00:00:00+00:00");
            stage_row(&journal.conn, 2, "{", "2030-01-01T00:00:00+00:00");

            let refusal = journal
                .all_states()
                .expect_err("two undecodable rows cannot both be skipped");

            assert!(
                refusal.to_string().contains("task 2"),
                "the read went through the projection in id order, so the first row it could \
                 not decode is the lowest-numbered one: {refusal}"
            );
        }

        #[test]
        fn damage_in_one_tasks_row_does_not_blind_a_read_of_another() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            journal
                .put_state(TaskId::new(1), &TaskState::Queued)
                .expect("a fresh projection takes a state");
            stage_row(&journal.conn, 2, "{", "2026-09-17T12:00:00+00:00");

            assert_eq!(
                journal
                    .get_state(TaskId::new(1))
                    .expect("task 1's own row is legible")
                    .as_ref(),
                Some(&TaskState::Queued),
                "one damaged row stops the read that reaches it, and says nothing about the \
                 task beside it"
            );
            assert!(
                journal.all_states().is_err(),
                "the whole-projection read cannot skip the damaged task, because a projection \
                 with a task quietly missing from it is a run that lost a task"
            );
        }

        /// The commit the scripted task published, and the remote's own tip.
        const PUBLISHED: &str = "b42c45f";

        /// One step of a run, recorded the way the recorder records it: the event
        /// appended first, the projection moved second, and one
        /// [`Journal::put_state`] per accepted transition.
        ///
        /// A projection compared against a rebuild has to have been written as a run
        /// writes it, or the comparison is between a replay and a shape no writer
        /// produces. The state is not handed in — it is what [`crate::apply`]
        /// concludes, which is the same function a replay folds with.
        fn record(journal: &mut Journal, task: Option<TaskId>, event: &EventKind) {
            journal
                .append(task, event)
                .expect("a legal event appends, transition or not");
            let Some(id) = task else {
                return;
            };
            let held = journal
                .get_state(id)
                .expect("one task's own row is always readable");
            let to = apply(&held.unwrap_or(TaskState::Queued), event)
                .expect("the scripted run is a legal walk of the machine");
            journal
                .put_state(id, &to)
                .expect("the projection moves after the event it answers");
        }

        /// Three tasks whose records interleave, and one event about the queue.
        ///
        /// Task 1 goes the whole way to done, task 2 is caught by a signal mid-`Red`
        /// and stays parked, task 3 was queued and never started: three different
        /// answers, because a fold that gets the happy path right can still lose the
        /// task that stopped early and the one that never began. Task 1's records are
        /// split up by the other two on purpose — a replay that kept two tasks in one
        /// accumulator would be caught by nothing else here — and the queue-level
        /// event sits between them, where a fold that read a `NULL` task as some
        /// task would trip over it.
        fn record_a_run(journal: &mut Journal) {
            let (one, two, three) = (TaskId::new(1), TaskId::new(2), TaskId::new(3));
            let first = AttemptId::new(1);
            let script = [
                (
                    Some(three),
                    EventKind::TaskQueued {
                        title: "three".to_owned(),
                    },
                ),
                (
                    Some(one),
                    EventKind::TaskQueued {
                        title: "one".to_owned(),
                    },
                ),
                (Some(one), EventKind::PreflightStarted),
                (
                    Some(two),
                    EventKind::TaskQueued {
                        title: "two".to_owned(),
                    },
                ),
                (None, EventKind::Resumed),
                (
                    Some(one),
                    EventKind::PhaseEntered {
                        attempt: first,
                        phase: Phase::Implement,
                    },
                ),
                (Some(two), EventKind::PreflightStarted),
                (
                    Some(two),
                    EventKind::PhaseEntered {
                        attempt: first,
                        phase: Phase::Red,
                    },
                ),
                (
                    Some(one),
                    EventKind::AgentOutput {
                        attempt: first,
                        stream: Stream::Stdout,
                        text: "the fold is written".to_owned(),
                    },
                ),
                (Some(two), EventKind::Interrupted { phase: Phase::Red }),
                (Some(one), EventKind::VerifyPassed { attempt: first }),
                (
                    Some(one),
                    EventKind::PublishVerified {
                        commit: PUBLISHED.to_owned(),
                        remote_sha: PUBLISHED.to_owned(),
                    },
                ),
                (
                    Some(one),
                    EventKind::TaskDone {
                        commit: PUBLISHED.to_owned(),
                    },
                ),
            ];
            for (task, event) in &script {
                record(journal, *task, event);
            }
        }

        /// The projection as the table holds it, less the stamp: one
        /// `(task id, state JSON)` pair per row, in id order.
        ///
        /// The stamp is left out because a rebuild restamps every row it writes (see
        /// [`Journal::rebuild_state`]), so the pair is everything the row says about
        /// the task — which is exactly what a rebuild has to get right.
        fn state_texts(conn: &Connection) -> Vec<(i64, String)> {
            stored_rows(conn)
                .into_iter()
                .map(|row| (row.task_id, row.state_json))
                .collect()
        }

        /// The sequence numbers `events` holds, in the order the journal gave them.
        fn event_sequences(conn: &Connection) -> Vec<i64> {
            let mut statement = conn
                .prepare("SELECT seq FROM events ORDER BY seq")
                .expect("events is always readable");
            statement
                .query_map([], |row| row.get(0))
                .expect("events is readable")
                .collect::<rusqlite::Result<Vec<i64>>>()
                .expect("every sequence reads as the integer the schema declares")
        }

        /// The `AUTOINCREMENT` counter `events` has spent up to.
        fn counter(conn: &Connection) -> i64 {
            conn.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'events'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("the counter table is readable")
            .unwrap_or(0)
        }

        #[test]
        fn a_rebuilt_projection_is_exactly_the_projection_the_run_wrote() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);

            let written = journal
                .all_states()
                .expect("a run's own projection is readable");
            let written_rows = state_texts(&journal.conn);

            journal
                .rebuild_state()
                .expect("a journal can rebuild the projection it materialized");

            assert_eq!(
                journal
                    .all_states()
                    .expect("the rebuilt projection is readable"),
                written,
                "replaying every event lands every task on the state the incremental writes \
                 landed it on"
            );
            assert_eq!(
                state_texts(&journal.conn),
                written_rows,
                "and the rows hold the same JSON for the same tasks, so the agreement is about \
                 what the table holds rather than only about what a read chooses to show"
            );
            assert_eq!(
                journal
                    .get_state(TaskId::new(1))
                    .expect("task 1's row is readable")
                    .as_ref(),
                Some(&TaskState::Done),
                "the task that ran the whole path is rebuilt as done, not as whatever the \
                 record before its last one said"
            );
            assert_eq!(
                journal
                    .get_state(TaskId::new(2))
                    .expect("task 2's row is readable")
                    .as_ref(),
                Some(&TaskState::Paused {
                    reason: PauseReason::Interrupted,
                    resume_to: Box::new(TaskState::Running {
                        attempt: AttemptId::new(1),
                        phase: Phase::Red,
                    }),
                }),
                "the task a signal caught is rebuilt parked, holding the phase it stopped in"
            );
            assert_eq!(
                journal
                    .get_state(TaskId::new(3))
                    .expect("task 3's row is readable")
                    .as_ref(),
                Some(&TaskState::Queued),
                "and a task that was only ever queued has a row, because the run wrote one for \
                 it and a replay owes the same answer"
            );
        }

        #[test]
        fn a_projection_dropped_whole_comes_back_from_the_events_alone() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);
            let written = journal
                .all_states()
                .expect("a run's own projection is readable");

            journal
                .conn
                .execute("DELETE FROM task_state", [])
                .expect("a projection can be dropped: that is what makes it a projection");
            assert!(
                journal
                    .all_states()
                    .expect("a projection with no rows is readable")
                    .is_empty(),
                "the drop is the whole of what is lost, so the table really is empty before \
                 the rebuild rather than being compared against a projection still standing"
            );
            assert_eq!(
                journal
                    .get_state(TaskId::new(1))
                    .expect("an absent row is an answer, not a failure"),
                None,
                "and one task reads back as absence, the way a task nobody has run does"
            );

            journal
                .rebuild_state()
                .expect("the events alone rebuild what was dropped");

            assert_eq!(
                journal
                    .all_states()
                    .expect("the rebuilt projection is readable"),
                written,
                "nothing but the journal was needed to get every state back, which is the \
                 claim `docs/DESIGN.md` makes when it calls `task_state` rebuildable by replay"
            );
        }

        #[test]
        fn a_replay_that_refuses_an_event_reports_its_sequence_and_writes_nothing() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);
            let written = journal
                .all_states()
                .expect("a run's own projection is readable");
            let written_rows = stored_rows(&journal.conn);

            journal
                .append(
                    Some(TaskId::new(4)),
                    &EventKind::TaskQueued {
                        title: "four".to_owned(),
                    },
                )
                .expect("the journal records what it is told");
            let offending = journal
                .append(
                    Some(TaskId::new(4)),
                    &EventKind::TaskDone {
                        commit: PUBLISHED.to_owned(),
                    },
                )
                .expect("refusing a transition is not an append's job, so the row is written");

            let refusal = journal.rebuild_state().expect_err(
                "a journal the state machine cannot replay cannot come back as a projection",
            );

            assert!(
                matches!(&refusal, Error::Corrupt { seq: Some(sequence), .. } if *sequence == offending.get()),
                "the refusal names the record the replay could not use, which is the one \
                 thing a human needs to go and read: {refusal}"
            );
            assert!(
                refusal
                    .to_string()
                    .contains(&format!("seq {}", offending.get())),
                "and it says so in words, not only in a field: {refusal}"
            );
            assert!(
                refusal
                    .to_string()
                    .contains("illegal transition from `Queued` on event `TaskDone`"),
                "beside the sequence it says which state refused which event, because a \
                 sequence alone says where to look and not what was wrong: {refusal}"
            );
            assert_eq!(
                journal
                    .all_states()
                    .expect("the projection a failed rebuild left is readable"),
                written,
                "a replay that cannot finish reports rather than stops: the projection is the \
                 one the run left behind, not the prefix the fold had reached"
            );
            assert_eq!(
                stored_rows(&journal.conn),
                written_rows,
                "which includes every stamp: nothing was rewritten on the way to the refusal"
            );
            assert_eq!(
                journal
                    .get_state(TaskId::new(4))
                    .expect("an absent row is an answer, not a failure"),
                None,
                "and the task the refusal is about is not half-projected either"
            );
        }

        #[test]
        fn a_row_for_a_task_the_journal_says_nothing_about_is_gone_after_a_rebuild() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);
            stage_row(
                &journal.conn,
                9,
                &stored_json(&TaskState::Done),
                "2026-09-17T12:00:00+00:00",
            );

            journal
                .rebuild_state()
                .expect("a projection holding a row no event explains is still rebuildable");

            assert_eq!(
                journal
                    .get_state(TaskId::new(9))
                    .expect("an absent row is an answer, not a failure"),
                None,
                "clearing the table is what makes a rebuild an answer rather than a patch: a \
                 row a hand put there, or one an older build wrote from events this file no \
                 longer holds, has no replay behind it and does not survive one"
            );
            assert_eq!(
                journal
                    .all_states()
                    .expect("the rebuilt projection is readable")
                    .keys()
                    .copied()
                    .collect::<Vec<TaskId>>(),
                vec![TaskId::new(1), TaskId::new(2), TaskId::new(3)],
                "the tasks the journal does speak of are all still there"
            );
        }

        #[test]
        fn a_rebuild_whose_write_is_refused_leaves_the_projection_it_found() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);
            let written = journal
                .all_states()
                .expect("a run's own projection is readable");
            let written_rows = stored_rows(&journal.conn);
            journal
                .conn
                .execute(
                    "CREATE TRIGGER task_state_refuse_two BEFORE INSERT ON task_state \
                     WHEN NEW.task_id = 2 \
                     BEGIN SELECT RAISE(ABORT, 'the test refuses the row'); END",
                    [],
                )
                .expect("a trigger can be put on the projection by a test");

            let refusal = journal
                .rebuild_state()
                .expect_err("a projection write the database refuses cannot rebuild anything");

            assert!(
                matches!(refusal, Error::Database(_)),
                "SQLite's own refusal comes back as it came: {refusal}"
            );
            assert_eq!(
                journal
                    .all_states()
                    .expect("the projection a refused rebuild left is readable"),
                written,
                "clearing and refilling the projection is one transaction, so a write refused \
                 partway through leaves every task holding the state it had rather than an \
                 empty table and two rows"
            );
            assert_eq!(
                stored_rows(&journal.conn),
                written_rows,
                "and the rows themselves, stamps included, are the ones the run wrote"
            );
        }

        #[test]
        fn a_rebuild_records_no_event_and_spends_no_sequence() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);
            let rows = event_sequences(&journal.conn);
            let spent = counter(&journal.conn);

            journal
                .rebuild_state()
                .expect("a journal can rebuild its own projection");

            assert_eq!(
                event_sequences(&journal.conn),
                rows,
                "a rebuild reads the journal and rewrites the projection; it records nothing, \
                 because what a run did is already in `events` and a replay of it is not a new \
                 fact about the run"
            );
            assert_eq!(
                counter(&journal.conn),
                spent,
                "and it spends no sequence number, so the next event is the one after the last \
                 the run wrote"
            );
            let next = journal
                .append(None, &EventKind::PreflightStarted)
                .expect("the journal still appends after being rebuilt");
            assert_eq!(
                next.get(),
                u64::try_from(spent + 1).expect("a scratch journal stays inside a sequence number"),
                "which is what the next record's number is the proof of"
            );
        }

        #[test]
        fn an_event_about_the_queue_moves_no_task_in_a_rebuild() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            let task = TaskId::new(1);
            record(
                &mut journal,
                Some(task),
                &EventKind::TaskQueued {
                    title: "one".to_owned(),
                },
            );
            record(
                &mut journal,
                None,
                &EventKind::TaskCancelled {
                    reason: "the run was stopped".to_owned(),
                },
            );
            record(&mut journal, Some(task), &EventKind::PreflightStarted);

            journal
                .rebuild_state()
                .expect("a journal holding queue-level events is still replayable");

            assert_eq!(
                journal
                    .get_state(task)
                    .expect("the task's row is readable")
                    .as_ref(),
                Some(&TaskState::Preflight),
                "an event naming no task belongs to no accumulator: it cancelled nothing, and \
                 the task is where its own two records put it"
            );
        }

        #[test]
        fn rebuilding_a_journal_that_holds_no_events_leaves_an_empty_projection() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());

            journal
                .rebuild_state()
                .expect("an empty journal has an empty projection, and rebuilding it is ordinary");

            assert!(
                journal
                    .all_states()
                    .expect("a projection with no rows is readable")
                    .is_empty(),
                "nothing was replayed, so nothing is projected — an empty projection is the \
                 right answer here and not a reason to refuse"
            );
            assert_eq!(
                journal
                    .get_state(TaskId::new(1))
                    .expect("an absent row is an answer, not a failure"),
                None,
                "and a task the journal has never heard of still reads as absence"
            );
        }

        #[test]
        fn a_rebuild_restamps_every_row_it_rewrites() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);
            let before = stored_rows(&journal.conn);
            std::thread::sleep(Duration::from_millis(2));

            journal
                .rebuild_state()
                .expect("a journal can rebuild its own projection");

            let after = stored_rows(&journal.conn);
            assert_eq!(after.len(), before.len(), "one row per task, as before");
            for (index, row) in after.iter().enumerate() {
                let rebuilt = OffsetDateTime::parse(&row.updated_at, &Rfc3339)
                    .expect("the stamp is the RFC 3339 UTC instant the column documents");
                let original = OffsetDateTime::parse(&before[index].updated_at, &Rfc3339)
                    .expect("the stamp a run wrote is spelled the same way");
                assert!(
                    rebuilt > original,
                    "task {}'s row is dated when this file rebuilt it, not when the run wrote \
                     it: an operator deciding whether to trust the projection needs to be able \
                     to tell one from the other",
                    row.task_id
                );
            }
        }

        #[test]
        fn rebuilding_twice_leaves_the_projection_the_first_rebuild_produced() {
            let parent = scratch();
            let mut journal = open_journal(parent.path());
            record_a_run(&mut journal);

            journal
                .rebuild_state()
                .expect("a journal can rebuild its own projection");
            let once = journal
                .all_states()
                .expect("the rebuilt projection is readable");
            let once_rows = state_texts(&journal.conn);

            journal
                .rebuild_state()
                .expect("and it can do it again, which is what makes a rebuild safe to re-run");

            assert_eq!(
                journal
                    .all_states()
                    .expect("the second projection is readable"),
                once,
                "a second replay of the same journal moves no task, so a rebuild is a repair \
                 someone may run twice rather than an operation with a first-time-only effect"
            );
            assert_eq!(
                state_texts(&journal.conn),
                once_rows,
                "and the rows are byte-identical, because the fold starts from `Queued` every \
                 time and not from what the table happens to hold"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CREATE_META_TABLE, EventRow, EventSeq, Journal, NANOSECONDS_PER_SECOND, SCHEMA_VERSION,
        SCHEMA_VERSION_KEY, decode_event, event_sequence, journal_path, reading_nanos, stamp_text,
    };
    use crate::{
        AttemptId, Error, Event, EventKind, FailureClass, PauseReason, Phase, Project, Recovery,
        Result, Stream, TaskId,
    };
    use rusqlite::{Connection, OptionalExtension as _, ffi::ErrorCode, params};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};
    use tempfile::{TempDir, tempdir};
    use time::format_description::well_known::Rfc3339;
    use time::macros::datetime;
    use time::{OffsetDateTime, UtcOffset};

    /// Every object a journal owns, as `sqlite_master` describes it, ordered by
    /// object name the way the test query orders them. The two triggers are the
    /// guards that make `events` append-only in the file rather than in this
    /// module's good intentions.
    const SCHEMA_OBJECTS: [&str; 7] = [
        "table events",
        "trigger events_refuse_delete",
        "trigger events_refuse_update",
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
                 WHERE type IN ('table', 'index', 'trigger') \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
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

    /// The triggers attached to one table, by name.
    fn triggers_on(conn: &Connection, table: &str) -> Vec<String> {
        let mut statement = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name",
            )
            .expect("sqlite_master is always readable");
        statement
            .query_map(params![table], |row| row.get::<_, String>(0))
            .expect("sqlite_master is readable")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("every trigger is named in text")
    }

    /// What a refusal by an append-only guard has to look like: SQLite's own
    /// constraint violation, quoting the words the guard was written with. A
    /// statement refused for some other reason — a typo, a missing table — would
    /// leave the same unchanged row and prove nothing, so both halves are
    /// asserted for every refused mutation below.
    fn assert_refused_by_an_append_only_guard(refused: &rusqlite::Error, guard_words: &str) {
        assert_eq!(
            refused.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation),
            "an append-only guard refuses with `RAISE(ABORT, ...)`, which SQLite reports as a \
             constraint violation; anything else was refused for some other reason: {refused}"
        );
        assert!(
            refused.to_string().contains(guard_words),
            "the refusal quotes the guard's own words, so a reader learns which rule the \
             statement broke rather than decoding a bare `error`: {refused}"
        );
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

    /// Move the `events` counter forward without writing a row, which is the state
    /// an interrupted commit leaves behind when its tail record is lost: the
    /// sequence is spent, the record is not in the file.
    ///
    /// Staged in the counter rather than by deleting a row, because the schema now
    /// refuses a `DELETE` against `events`, and staging a loss by performing the
    /// mutation the guard exists to forbid would test the wrong thing.
    fn spend_the_next_sequence(conn: &Connection) {
        conn.execute(
            "UPDATE sqlite_sequence SET seq = seq + 1 WHERE name = 'events'",
            [],
        )
        .expect("the number a lost record spent can be staged");
    }

    /// One event as the `events` table holds it, in the types the schema
    /// declares. Read back rather than assumed: every append assertion below is
    /// against what the file ended up carrying.
    #[derive(Debug, PartialEq)]
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
            "the three tables, the index and the two append-only guards of `docs/DESIGN.md` \
             Database schema, and nothing else, all created the first time"
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
            // behind: the next sequence is spent, and the record that spent it
            // never reached the file. Production code never mutates `events`, and
            // the schema now refuses the `DELETE` this test used to stage the loss
            // with, so the loss is staged where a lost record leaves it: in the
            // counter. `AUTOINCREMENT` is what keeps the next record from claiming
            // a sequence a run has already reported to a human.
            spend_the_next_sequence(&opened.conn);
        }
        let reopened = Journal::open(&path).expect("the journal reopens");
        append_event(&reopened.conn, "PreflightPassed");

        assert_eq!(
            sequences(&reopened.conn),
            [1, 2, 4],
            "sequence 3 is spent for good: a journal whose numbering repeats would read two \
             records as one thing, and the hole a lost record leaves is never filled by a later \
             one"
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
    fn the_events_table_carries_the_two_append_only_guards_and_no_other_table_carries_one() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        assert_eq!(
            triggers_on(&journal.conn, "events"),
            ["events_refuse_delete", "events_refuse_update"],
            "both halves of mutation are refused on the table the guards belong to: a rewrite \
             and a removal"
        );
        for table in ["tasks", "task_state", "meta"] {
            assert!(
                triggers_on(&journal.conn, table).is_empty(),
                "`{table}` is the queue or a projection, and rewriting it is ordinary work: the \
                 guard is the journal's, not a rule imposed on every table"
            );
        }
    }

    /// The `UPDATE` half of what an append-only journal has to refuse: the statement
    /// is answered by SQLite with an error, and the row it aimed at is the row that
    /// was already there.
    #[test]
    fn an_update_of_a_stored_event_is_refused_and_the_row_is_left_as_it_was() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let appended = journal
            .append(
                Some(TaskId::new(7)),
                &EventKind::TaskQueued {
                    title: "the record a mutation is aimed at".to_owned(),
                },
            )
            .expect("the event appends");
        let kept = stored_at(&journal.conn, appended);
        let before = stored_events(&journal.conn);

        let refused = journal
            .conn
            .execute(
                "UPDATE events SET ts = ?1, kind = ?2, payload = ?3 WHERE seq = ?4",
                params![AN_INSTANT, "TaskDone", r#"{"rewritten":true}"#, kept.seq],
            )
            .expect_err("a stored event is evidence, so rewriting it is not an operation");

        assert_refused_by_an_append_only_guard(&refused, "never updated");
        assert_eq!(
            stored_events(&journal.conn),
            before,
            "the row the statement named is the row that was there, every column of it"
        );
    }

    /// The `DELETE` half of the same refusal, measured against the whole table so a
    /// guard that refused the named row while dropping another one cannot pass.
    #[test]
    fn a_delete_of_a_stored_event_is_refused_and_the_row_is_left_as_it_was() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let appended = journal
            .append(
                None,
                &EventKind::TaskQueued {
                    title: "the record a removal is aimed at".to_owned(),
                },
            )
            .expect("the event appends");
        let kept = stored_at(&journal.conn, appended);
        let before = stored_events(&journal.conn);

        let refused = journal
            .conn
            .execute("DELETE FROM events WHERE seq = ?1", params![kept.seq])
            .expect_err("what a run did cannot be taken back out of the file");

        assert_refused_by_an_append_only_guard(&refused, "never deleted");
        assert_eq!(
            stored_events(&journal.conn),
            before,
            "neither the row the removal named nor any other left the table"
        );
    }

    /// A mutation naming no row is the blunt way to rewrite a journal, and it is
    /// answered the same way: refused, undone rather than applied as far as it got,
    /// and with the journal left open for the next append.
    #[test]
    fn a_whole_table_mutation_is_refused_and_costs_the_journal_nothing() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        journal
            .append(
                None,
                &EventKind::TaskQueued {
                    title: "first".to_owned(),
                },
            )
            .expect("the first event appends");
        journal
            .append(Some(TaskId::new(1)), &EventKind::PreflightStarted)
            .expect("the second event appends");
        let before = stored_events(&journal.conn);

        for (statement, guard_words) in [
            ("UPDATE events SET kind = 'TaskDone'", "never updated"),
            ("DELETE FROM events", "never deleted"),
        ] {
            let refused = journal
                .conn
                .execute(statement, [])
                .expect_err("a statement that names no row still names the guarded table");
            assert_refused_by_an_append_only_guard(&refused, guard_words);
        }

        assert_eq!(
            stored_events(&journal.conn),
            before,
            "`RAISE(ABORT)` aborts the statement and undoes the rows it had already reached, so \
             a blunt rewrite leaves the journal exactly as it found it"
        );

        let next = journal
            .append(None, &EventKind::Resumed)
            .expect("a refused mutation leaves the journal usable");
        assert_eq!(
            stored_at(&journal.conn, next).kind,
            "Resumed",
            "the append that follows a refusal is an ordinary append, after the events the \
             guards kept"
        );
        assert_eq!(
            stored_events(&journal.conn).len(),
            3,
            "two guarded events and the one that followed them: a refusal wrote nothing and \
             cost no row"
        );
    }

    /// The guards belong to the file, not to the connection that created them, so a
    /// handle this module never opened meets the same refusal — which is the point:
    /// append-only here is a property of the journal, not a courtesy `Journal`
    /// observes.
    #[test]
    fn the_guards_are_in_the_file_so_a_fresh_connection_is_refused_too() {
        let parent = scratch();
        let path = journal_file(parent.path());
        let mut journal = Journal::open(&path).expect("a new journal");
        let appended = journal
            .append(
                None,
                &EventKind::TaskQueued {
                    title: "written by one handle, guarded for every handle".to_owned(),
                },
            )
            .expect("the event appends");
        let kept = stored_at(&journal.conn, appended);
        drop(journal);

        let reopened = Journal::open(&path).expect("the journal reopens");
        let refused = reopened
            .conn
            .execute(
                "UPDATE events SET payload = '{}' WHERE seq = ?1",
                params![kept.seq],
            )
            .expect_err("a second connection to the same file meets the same guards");

        assert_refused_by_an_append_only_guard(&refused, "never updated");
        assert_eq!(
            stored_at(&reopened.conn, appended).payload,
            kept.payload,
            "the row the fresh handle could not rewrite is the row the first one wrote"
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

    /// Write one `events` row exactly as the caller spells it, `seq` included.
    ///
    /// The append pipeline derives its own sequence, instant and payload, which
    /// is exactly what its tests hold it to — so a read test that needs a row
    /// whose sequence and instant disagree about the order, or a column holding
    /// something no append could have written, stages the row itself. Naming
    /// `seq` is how a row lands out of write order: it is the rowid.
    fn stage_row(
        conn: &Connection,
        seq: i64,
        ts: &str,
        task_id: Option<i64>,
        kind: &str,
        payload: &str,
    ) {
        conn.execute(
            "INSERT INTO events (seq, ts, task_id, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq, ts, task_id, kind, payload],
        )
        .expect("a row the test spells out can be staged");
    }

    /// The sequence numbers a read handed back, in the order it handed them back.
    /// The order is half of what the three reads promise, so almost every read
    /// assertion below is against this rather than against a set.
    fn read_sequences(events: &[Event]) -> Vec<u64> {
        events.iter().map(|event| event.seq.get()).collect()
    }

    /// The `events` counter, which is what a lost record leaves spent.
    fn counter(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'events'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("the counter table is readable")
        .unwrap_or(0)
    }

    /// A row an append could have written, which a test then breaks in exactly
    /// one column, so every refusal below has one cause and not three.
    fn a_sound_row() -> EventRow {
        EventRow {
            seq: 1,
            ts: "2026-09-17T12:00:00Z".to_owned(),
            task_id: Some(7),
            kind: "TaskQueued".to_owned(),
            payload: r#"{"kind":"TaskQueued","title":"Read events back"}"#.to_owned(),
        }
    }

    #[test]
    fn a_fresh_journal_reads_back_no_events() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

        assert!(
            journal
                .events()
                .expect("an empty journal is readable rather than an error")
                .is_empty(),
            "nothing was appended, so the whole journal is nothing — an empty answer, not a \
             refusal to answer"
        );
        assert!(
            journal
                .events_for(TaskId::new(1))
                .expect("a task with no events is readable")
                .is_empty(),
            "a task the queue has not reached yet has no events, which the journal knows rather \
             than guesses"
        );
        assert!(
            journal
                .events_since(EventSeq::new(0))
                .expect("a cursor before the first event is readable")
                .is_empty(),
            "sequence 0 is the cursor a reader that has seen nothing carries, and a fresh \
             journal still has nothing to hand it"
        );
    }

    #[test]
    fn one_appended_event_reads_back_with_the_four_facts_the_journal_stamped() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let appended = EventKind::TaskQueued {
            title: "Read events back".to_owned(),
        };

        let before = OffsetDateTime::from(SystemTime::now());
        let seq = journal
            .append(Some(TaskId::new(7)), &appended)
            .expect("one event appends");
        let read = journal.events().expect("the journal reads back");
        let after = OffsetDateTime::from(SystemTime::now());

        assert_eq!(read.len(), 1, "one append, one event read back");
        let event = read.first().expect("the one appended event");
        assert_eq!(
            event.seq, seq,
            "the record carries the sequence `append` returned, so a reader can quote it back"
        );
        assert_eq!(
            event.task_id,
            Some(TaskId::new(7)),
            "the record carries the task it was appended against"
        );
        assert_eq!(
            event.kind, appended,
            "the payload decodes into the catalog entry that was appended, every field of it"
        );
        assert!(
            event.ts >= before && event.ts <= after,
            "the instant read back is the one the append stamped, bounded between the two clock \
            reads around the call: {} not in {before} .. {after}",
            event.ts
        );
        assert_eq!(
            event.ts.offset(),
            UtcOffset::UTC,
            "the column's one spelling is read back at the UTC offset"
        );

        assert_eq!(
            journal
                .events_for(TaskId::new(7))
                .expect("the task's own read works"),
            read,
            "one task's events are the whole journal when the journal holds one task's event"
        );
        assert!(
            journal
                .events_for(TaskId::new(8))
                .expect("a task nothing appended to is readable")
                .is_empty(),
            "an empty task is an empty answer, not a NotFound: the queue numbers four billion \
             positions and nearly all of them are empty at any moment"
        );
        assert_eq!(
            journal
                .events_since(EventSeq::new(0))
                .expect("a cursor before the first event works"),
            read,
            "a reader holding sequence 0 has seen nothing, so the one event is ahead of it"
        );
        assert!(
            journal
                .events_since(seq)
                .expect("a cursor at the newest event works")
                .is_empty(),
            "a cursor at the newest event has seen everything the journal holds"
        );
    }

    #[test]
    fn many_events_read_back_in_the_order_the_journal_numbered_them() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let written = [
            (
                Some(TaskId::new(1)),
                EventKind::TaskQueued {
                    title: "one".to_owned(),
                },
            ),
            (None, EventKind::PreflightStarted),
            (
                Some(TaskId::new(2)),
                EventKind::TaskQueued {
                    title: "two".to_owned(),
                },
            ),
            (
                Some(TaskId::new(1)),
                EventKind::TaskDone {
                    commit: SHA.to_owned(),
                },
            ),
        ];
        for (task, kind) in &written {
            journal.append(*task, kind).expect("every entry appends");
        }

        let read = journal.events().expect("the four events read back");
        assert_eq!(
            read_sequences(&read),
            vec![1, 2, 3, 4],
            "the whole journal comes back numbered from one, once each"
        );
        let observed: Vec<(u64, Option<TaskId>, EventKind)> = read
            .iter()
            .map(|event| (event.seq.get(), event.task_id, event.kind.clone()))
            .collect();
        assert_eq!(
            observed,
            vec![
                (
                    1,
                    Some(TaskId::new(1)),
                    EventKind::TaskQueued {
                        title: "one".to_owned(),
                    },
                ),
                (2, None, EventKind::PreflightStarted),
                (
                    3,
                    Some(TaskId::new(2)),
                    EventKind::TaskQueued {
                        title: "two".to_owned(),
                    },
                ),
                (
                    4,
                    Some(TaskId::new(1)),
                    EventKind::TaskDone {
                        commit: SHA.to_owned(),
                    },
                ),
            ],
            "each record carries its own task and its own entry — a read that paired a sequence \
             with the wrong row would still hand back four events and four numbers"
        );

        drop(journal);
        let reopened =
            Journal::open(&journal_file(parent.path())).expect("the journal opens a second time");
        assert_eq!(
            read_sequences(&reopened.events().expect("the reopened journal reads back")),
            vec![1, 2, 3, 4],
            "reopening reads the same records in the same order: the counter keeps a sequence \
             from coming back and the rows keep their numbers"
        );
    }

    #[test]
    fn events_are_read_in_sequence_order_and_not_in_the_order_they_were_stamped() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        stage_row(
            &journal.conn,
            4,
            "2026-09-17T00:00:00Z",
            Some(1),
            "PreflightStarted",
            r#"{"kind":"PreflightStarted"}"#,
        );
        stage_row(
            &journal.conn,
            2,
            "2026-09-17T12:00:00Z",
            Some(1),
            "Resumed",
            r#"{"kind":"Resumed"}"#,
        );
        stage_row(
            &journal.conn,
            9,
            "2026-09-17T06:00:00Z",
            None,
            "PreflightPassed",
            r#"{"kind":"PreflightPassed","base_sha":"0b78d3f1c2a4"}"#,
        );

        let read = journal.events().expect("the staged rows read back");
        let observed: Vec<(u64, OffsetDateTime)> = read
            .iter()
            .map(|event| (event.seq.get(), event.ts))
            .collect();
        assert_eq!(
            observed,
            vec![
                (2, datetime!(2026-09-17 12:00:00 UTC)),
                (4, datetime!(2026-09-17 00:00:00 UTC)),
                (9, datetime!(2026-09-17 06:00:00 UTC)),
            ],
            "the answer is ordered by `seq` — 2, 4, 9 — while the instants on those same rows \
             run noon, midnight, six. Ordered by `ts` it would be 4, 9, 2; the order the rows \
             were staged in is 4, 2, 9, so only the sequence explains the answer"
        );
    }

    #[test]
    fn events_for_reads_one_tasks_events_and_neither_another_tasks_nor_the_queues() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        let queued = journal
            .append(
                Some(TaskId::new(1)),
                &EventKind::TaskQueued {
                    title: "one".to_owned(),
                },
            )
            .expect("the first event appends");
        let queue_level = journal
            .append(None, &EventKind::PreflightStarted)
            .expect("a queue-level event appends");
        let phase = journal
            .append(
                Some(TaskId::new(1)),
                &EventKind::PhaseEntered {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
            )
            .expect("the third event appends");
        let other = journal
            .append(
                Some(TaskId::new(2)),
                &EventKind::TaskQueued {
                    title: "two".to_owned(),
                },
            )
            .expect("the fourth event appends");
        let done = journal
            .append(
                Some(TaskId::new(1)),
                &EventKind::TaskDone {
                    commit: SHA.to_owned(),
                },
            )
            .expect("the fifth event appends");

        let one = journal
            .events_for(TaskId::new(1))
            .expect("task 1 has events to read");
        assert_eq!(
            read_sequences(&one),
            vec![queued.get(), phase.get(), done.get()],
            "one task's records come back in sequence order, gaps and all: its own 1, 3 and 5, \
             with the other task's 4 and the queue's 2 left out"
        );
        assert_eq!(
            read_sequences(
                &journal
                    .events_for(TaskId::new(2))
                    .expect("task 2 has one event")
            ),
            vec![other.get()],
            "the other task's single record is its own, and only its own"
        );
        assert!(
            journal
                .events_for(TaskId::new(3))
                .expect("task 3 was never queued")
                .is_empty(),
            "a task with no records is an empty read"
        );
        assert!(
            !read_sequences(&one).contains(&queue_level.get()),
            "the queue-level record belongs to no task, so it is in `events` and in no per-task \
             read: sequence {queue_level} is nowhere in {one:?}"
        );
        assert!(
            read_sequences(&journal.events().expect("all five read back"))
                .contains(&queue_level.get()),
            "and that is a decision about the per-task read, not a row the journal never held"
        );
    }

    #[test]
    fn events_since_hands_back_only_what_its_cursor_has_not_seen() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        for task in [Some(TaskId::new(1)), None, Some(TaskId::new(2)), None] {
            journal
                .append(task, &EventKind::PreflightStarted)
                .expect("an event appends");
        }

        assert_eq!(
            read_sequences(
                &journal
                    .events_since(EventSeq::new(0))
                    .expect("a cursor before the first event works")
            ),
            vec![1, 2, 3, 4],
            "sequence 0 names no record, so a reader holding it has seen nothing"
        );
        assert_eq!(
            read_sequences(
                &journal
                    .events_since(EventSeq::new(2))
                    .expect("a cursor in the middle works")
            ),
            vec![3, 4],
            "the cursor is the newest sequence the reader already holds, so its own record is \
             not handed back a second time — a poll that re-reads what it answered with shows \
             every screen the same event twice"
        );
        assert!(
            journal
                .events_since(EventSeq::new(4))
                .expect("a cursor at the newest event works")
                .is_empty(),
            "nothing is ahead of the newest event"
        );
    }

    #[test]
    fn a_cursor_that_names_a_lost_record_still_reports_what_came_after_it() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        journal
            .append(None, &EventKind::PreflightStarted)
            .expect("the first event appends");
        spend_the_next_sequence(&journal.conn);
        journal
            .append(
                Some(TaskId::new(1)),
                &EventKind::TaskDone {
                    commit: SHA.to_owned(),
                },
            )
            .expect("the event after the lost one appends");

        assert_eq!(
            read_sequences(
                &journal
                    .events()
                    .expect("the two surviving events read back")
            ),
            vec![1, 3],
            "the lost record left its sequence spent and its row absent, which a read reports \
             as the gap it is rather than filling in"
        );
        assert_eq!(
            read_sequences(
                &journal
                    .events_since(EventSeq::new(2))
                    .expect("a cursor naming a lost record works")
            ),
            vec![3],
            "a cursor does not have to name a record the journal still holds: the reader that \
             lost sequence 2 to an interrupted commit is still ahead of it, and 3 is what it \
             has not seen"
        );
    }

    #[test]
    fn a_cursor_wider_than_the_column_can_hold_reads_as_nothing_ahead_of_it() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        journal
            .append(None, &EventKind::PreflightStarted)
            .expect("an event appends");
        let widest = u64::try_from(i64::MAX).expect("the widest `INTEGER` is a sequence number");

        assert!(
            journal
                .events_since(EventSeq::new(widest))
                .expect("a cursor at the widest number the column holds is answerable")
                .is_empty(),
            "nothing above the widest number the column can hold is in it"
        );
        assert!(
            journal
                .events_since(EventSeq::new(u64::MAX))
                .expect("a cursor wider than the column is answerable too, not a refusal")
                .is_empty(),
            "no sequence the column can store is ahead of `u64::MAX`, so the honest answer is \
             the empty read rather than a conversion failure or a panic on the width"
        );
    }

    #[test]
    fn reading_events_back_writes_nothing_and_spends_no_sequence() {
        let parent = scratch();
        let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        journal
            .append(Some(TaskId::new(1)), &EventKind::Resumed)
            .expect("one event appends");

        for _ in 0..3 {
            journal.events().expect("the whole read works");
            journal
                .events_for(TaskId::new(1))
                .expect("the per-task read works");
            journal
                .events_since(EventSeq::new(0))
                .expect("the cursor read works");
        }

        assert_eq!(
            stored_events(&journal.conn).len(),
            1,
            "nine reads added no row to the table they were reading"
        );
        assert_eq!(
            counter(&journal.conn),
            1,
            "and left the sequence counter where the append left it: a read cannot spend a \
             number, because a spent number is an event a later reader waits for"
        );
        let next = journal
            .append(
                Some(TaskId::new(1)),
                &EventKind::Interrupted {
                    phase: Phase::Verify,
                },
            )
            .expect("the journal still appends after being read");
        assert_eq!(
            next.get(),
            2,
            "which is what the next event's number proves"
        );
    }

    #[test]
    fn a_row_numbered_below_the_first_event_is_read_as_damage() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        stage_row(
            &journal.conn,
            -1,
            AN_INSTANT,
            None,
            "Resumed",
            r#"{"kind":"Resumed"}"#,
        );

        let error = journal
            .events()
            .expect_err("a row whose sequence cannot be a count of events is not an event");

        assert!(
            matches!(error, Error::Corrupt { seq: None, .. }),
            "the number is the record's location and it is the number that is wrong, so there \
             is no sequence left to name: {error}"
        );
        assert!(
            error.to_string().contains("-1"),
            "the report quotes the number it refused: {error}"
        );
    }

    #[test]
    fn a_row_whose_two_halves_name_different_entries_is_refused_as_damage() {
        let parent = scratch();
        let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
        stage_row(
            &journal.conn,
            1,
            AN_INSTANT,
            Some(3),
            "TaskQueued",
            r#"{"kind":"Resumed"}"#,
        );

        let error = journal.events_for(TaskId::new(3)).expect_err(
            "a row naming one entry in its column and another in its payload is neither of them",
        );
        let reported = error.to_string();

        assert!(
            matches!(error, Error::Corrupt { seq: Some(1), .. }),
            "the read got far enough to know which record it could not trust: {error}"
        );
        assert!(
            reported.contains("TaskQueued") && reported.contains("Resumed"),
            "the report quotes both halves, because saying which one to believe is not this \
             read's to decide: {reported}"
        );
    }

    #[test]
    fn a_sound_row_decodes_into_the_envelope_the_four_columns_describe() {
        let event = decode_event(a_sound_row()).expect("a row an append wrote decodes");

        assert_eq!(
            event,
            Event {
                seq: EventSeq::new(1),
                ts: datetime!(2026-09-17 12:00:00 UTC),
                task_id: Some(TaskId::new(7)),
                kind: EventKind::TaskQueued {
                    title: "Read events back".to_owned(),
                },
            },
            "each column becomes its own field: a row whose number became its task, or whose \
             text became its sequence, would not be the record that was written"
        );
    }

    #[test]
    fn an_instant_that_is_not_the_columns_one_spelling_is_damage_naming_the_row() {
        let mut row = a_sound_row();
        row.ts = "yesterday".to_owned();

        let error = decode_event(row)
            .expect_err("an instant the column cannot have come from is not an instant");

        assert!(
            matches!(error, Error::Corrupt { seq: Some(1), .. }),
            "the rest of the row read, so its sequence is known and worth naming: {error}"
        );
        assert!(
            error.to_string().contains("yesterday") && error.to_string().contains("RFC 3339"),
            "the report quotes the text and the spelling it broke: {error}"
        );
    }

    #[test]
    fn a_queue_position_wider_than_the_identifier_is_damage_naming_the_number() {
        let mut row = a_sound_row();
        row.task_id = Some(i64::from(u32::MAX) + 1);

        let error = decode_event(row)
            .expect_err("a task number no queue position can be is not a task number");

        assert!(
            matches!(error, Error::Corrupt { seq: Some(1), .. }),
            "the record is located even though its task is not: {error}"
        );
        assert!(
            error.to_string().contains("4294967296"),
            "the report quotes the number it refused: {error}"
        );
    }

    #[test]
    fn a_payload_that_is_not_one_json_object_is_damage_that_keeps_its_sequence() {
        let mut row = a_sound_row();
        row.payload = "{ kind: }".to_owned();

        let error =
            decode_event(row).expect_err("a payload that is not JSON is not a catalog entry");

        assert!(
            matches!(error, Error::Corrupt { seq: Some(1), .. }),
            "an unreadable payload does not make the row's location unknown: {error}"
        );
        assert!(
            error.to_string().contains("catalog entry"),
            "the report says what it could not read rather than only that it failed: {error}"
        );
    }

    #[test]
    fn a_payload_naming_an_entry_the_catalog_does_not_hold_is_damage() {
        let mut row = a_sound_row();
        row.kind = "GateFinished".to_owned();
        row.payload = r#"{"kind":"GateFinished","result":"Passed"}"#.to_owned();

        let error = decode_event(row)
            .expect_err("an entry the catalog does not define has no fields this build can read");

        assert!(
            matches!(error, Error::Corrupt { seq: Some(1), .. }),
            "the refusal is a value carrying the record's location: {error}"
        );
        assert!(
            error.to_string().contains("GateFinished"),
            "the report names the entry nobody defined: {error}"
        );
    }

    /// The streaming read: [`Journal::for_each_event`], which hands a record to its
    /// reader one at a time and keeps none of them.
    ///
    /// Every assertion below is made by a callback that holds numbers and never a
    /// collection, because that is the shape of the promise. A test that gathered
    /// the stream into a `Vec` in order to compare it would prove only that this
    /// journal fitted in memory — the thing the read exists to make unnecessary.
    mod streaming {
        use std::time::SystemTime;

        use time::{OffsetDateTime, UtcOffset, macros::datetime};

        use super::{AN_INSTANT, SHA, counter, journal_file, scratch, stage_row, stored_events};
        use crate::{Error, Event, EventKind, EventSeq, Journal, Phase, TaskId};

        /// How many records the read is measured against: far past what any screen
        /// shows, and small enough for a test to write one at a time.
        const TEN_THOUSAND: u64 = 10_000;

        /// What a reader can know about a stream while holding none of it: how many
        /// records arrived, which arrived first and last, and how many arrived no
        /// later than the one before.
        ///
        /// Four numbers and no collection is what makes the count an assertion about
        /// the read rather than an accident of the test. Together they are arithmetic:
        /// a stream that counted `n` records, each later than the one before, running
        /// from the first sequence to the last, delivered every record in that range
        /// exactly once — a repeat would cost a count, and a gap would widen the
        /// range past it.
        #[derive(Debug, Default)]
        struct Visited {
            seen: u64,
            first: Option<EventSeq>,
            last: Option<EventSeq>,
            out_of_order: u64,
        }

        impl Visited {
            /// Note one handed-back record: keep its number, drop the record.
            fn visit(&mut self, event: &Event) {
                match self.last {
                    Some(previous) if event.seq > previous => {}
                    Some(_) => self.out_of_order += 1,
                    None => self.first = Some(event.seq),
                }
                self.last = Some(event.seq);
                self.seen += 1;
            }
        }

        /// Append `count` events, one per journal sequence, in the order the journal
        /// numbers them.
        ///
        /// Every record the stream is measured against comes from
        /// [`Journal::append`], so the read is tested against the rows a run actually
        /// leaves behind rather than a test's idea of them.
        fn append_n(journal: &mut Journal, count: u64) {
            for index in 0..count {
                let position = u32::try_from(index % 8 + 1)
                    .expect("a queue position inside the width of an identifier");
                journal
                    .append(
                        Some(TaskId::new(position)),
                        &EventKind::TaskQueued {
                            title: format!("event {index}"),
                        },
                    )
                    .expect("an event appends");
            }
        }

        /// The refusal a callback raises, spelled once so an assertion can name whose
        /// error came back out of the read.
        fn reader_refused() -> Error {
            Error::Policy {
                detail: "the reader stopped at the record it was handed".to_owned(),
                paths: Vec::new(),
            }
        }

        /// One [`Event`] for [`Visited`] to be told about, differing from its
        /// siblings only in the sequence it carries.
        fn a_record_at(sequence: u64) -> Event {
            Event {
                seq: EventSeq::new(sequence),
                ts: datetime!(2026-09-17 12:00:00 UTC),
                task_id: Some(TaskId::new(1)),
                kind: EventKind::PreflightStarted,
            }
        }

        #[test]
        fn the_counter_a_reader_keeps_notices_a_record_that_arrived_no_later_than_its_neighbour() {
            // What the ten-thousand-event assertion claims is arithmetic: `n`
            // records, each later than the one before, running from the first
            // sequence to the last, are those records once each. That is worth
            // asserting only if this counter *notices* a record that arrived out of
            // order, so it is shown a backwards one and a repeat — a helper that saw
            // neither would make every ordering assertion here quietly vacuous.
            let mut visited = Visited::default();
            visited.visit(&a_record_at(2));
            visited.visit(&a_record_at(1));
            visited.visit(&a_record_at(1));

            assert_eq!(
                visited.seen, 3,
                "every record it was handed is counted, in order or not"
            );
            assert_eq!(
                visited.first,
                Some(EventSeq::new(2)),
                "the first record it was handed is the one it remembers as first"
            );
            assert_eq!(
                visited.out_of_order, 2,
                "the backwards record and the repeat are both counted, which is what makes the \
                 `0` elsewhere mean a stream that never went backwards and never repeated: \
                 {visited:?}"
            );
        }

        #[test]
        fn ten_thousand_events_stream_to_a_callback_that_counts_rather_than_stores() {
            let parent = scratch();
            let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            append_n(&mut journal, TEN_THOUSAND);

            let mut visited = Visited::default();
            journal
                .for_each_event(EventSeq::new(0), &mut |event| {
                    visited.visit(&event);
                    Ok(())
                })
                .expect("ten thousand records are readable one at a time");

            assert_eq!(
                visited.seen, TEN_THOUSAND,
                "the callback was handed every record the journal holds, and it holds no \
                 collection that could have grown to {} — the number is what the read delivered",
                visited.seen
            );
            assert_eq!(
                visited.first,
                Some(EventSeq::new(1)),
                "the stream opened on the first record the journal ever issued, not somewhere \
                 into it"
            );
            assert_eq!(
                visited.last,
                Some(EventSeq::new(TEN_THOUSAND)),
                "and closed on the newest record, so a caller that streamed from the front saw \
                 the whole journal rather than a prefix of it"
            );
            assert_eq!(
                visited.out_of_order, 0,
                "no record arrived at or before the one before it, so {} counts running from \
                 sequence 1 to sequence {TEN_THOUSAND} are those same {TEN_THOUSAND} records \
                 delivered once each — a repeat would have cost a count and a gap would have \
                 widened the range",
                visited.seen
            );
        }

        #[test]
        fn the_stream_starts_strictly_after_its_cursor_and_ends_at_the_newest_record() {
            let parent = scratch();
            let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            append_n(&mut journal, 4);

            for cursor in 0..=4u64 {
                let mut visited = Visited::default();
                journal
                    .for_each_event(EventSeq::new(cursor), &mut |event| {
                        visited.visit(&event);
                        Ok(())
                    })
                    .expect("a cursor anywhere in the journal is answerable");

                assert_eq!(
                    visited.seen,
                    4 - cursor,
                    "a reader holding sequence {cursor} has seen that record already, so the \
                     stream ahead of it holds {} records and not the {} ahead of it it was \
                     handed: {visited:?}",
                    4 - cursor,
                    4 - cursor,
                );
                assert_eq!(
                    visited.first,
                    (cursor < 4).then(|| EventSeq::new(cursor + 1)),
                    "the record a cursor names is the newest one its reader has seen, so the \
                     one after it opens the stream — or nothing does, past the newest record"
                );
            }
        }

        #[test]
        fn a_fresh_journal_streams_nothing_and_refuses_nothing() {
            let parent = scratch();
            let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");

            let mut visited = Visited::default();
            journal
                .for_each_event(EventSeq::new(0), &mut |event| {
                    visited.visit(&event);
                    Ok(())
                })
                .expect("a journal that holds nothing is readable rather than an error");

            assert_eq!(
                visited.seen, 0,
                "nothing was appended, so nothing arrived — an empty stream, not a refusal to \
                 answer"
            );
            assert_eq!(
                visited.first, None,
                "and there is no first record for a caller to be told about"
            );
        }

        #[test]
        fn a_cursor_wider_than_the_column_can_hold_streams_nothing() {
            let parent = scratch();
            let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            append_n(&mut journal, 3);

            for widest in [
                u64::try_from(i64::MAX).expect("the widest `INTEGER` is a sequence number"),
                u64::MAX,
            ] {
                let mut visited = Visited::default();
                journal
                    .for_each_event(EventSeq::new(widest), &mut |event| {
                        visited.visit(&event);
                        Ok(())
                    })
                    .expect("a cursor no record can be ahead of is answerable, not a refusal");

                assert_eq!(
                    visited.seen, 0,
                    "no sequence the `seq` column can store is ahead of {widest}, so the honest \
                     answer is the empty stream rather than a conversion failure"
                );
            }
        }

        #[test]
        fn the_stream_arrives_in_the_order_the_journal_numbered_its_records() {
            let parent = scratch();
            let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            // Four rows whose instants run backwards to their sequences: the newest
            // record carries the oldest clock reading, which is what a journal holds
            // whenever a clock moves (ADR-0016). An order taken from `ts` would hand
            // these back the other way round.
            for sequence in 1..=4i64 {
                stage_row(
                    &journal.conn,
                    sequence,
                    &format!("2026-09-1{}T12:00:00+00:00", 6 - sequence),
                    Some(1),
                    "PreflightStarted",
                    r#"{"kind":"PreflightStarted"}"#,
                );
            }

            let mut arrived = 1u64;
            journal
                .for_each_event(EventSeq::new(0), &mut |event| {
                    assert_eq!(
                        event.seq,
                        EventSeq::new(arrived),
                        "the {arrived}th record to arrive carries sequence {}, which is not the \
                         {arrived}th the journal issued",
                        event.seq,
                    );
                    arrived += 1;
                    Ok(())
                })
                .expect("four staged rows stream");

            assert_eq!(
                arrived, 5,
                "every staged row arrived, in sequence order, before the read returned"
            );
        }

        #[test]
        fn every_record_arrives_whole_with_its_task_and_its_catalog_entry() {
            let parent = scratch();
            let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            let stamped = [
                (
                    Some(TaskId::new(1)),
                    EventKind::TaskQueued {
                        title: "one".to_owned(),
                    },
                ),
                (None, EventKind::PreflightStarted),
                (
                    Some(TaskId::new(2)),
                    EventKind::TaskDone {
                        commit: SHA.to_owned(),
                    },
                ),
            ];
            let before = OffsetDateTime::from(SystemTime::now());
            for (task, kind) in &stamped {
                journal.append(*task, kind).expect("an event appends");
            }
            let after = OffsetDateTime::from(SystemTime::now());

            let mut arrived = 0u64;
            journal
                .for_each_event(EventSeq::new(0), &mut |event| {
                    let position =
                        usize::try_from(arrived).expect("three records fit inside a machine index");
                    let (task, kind) = &stamped[position];
                    assert_eq!(
                        event.seq,
                        EventSeq::new(arrived + 1),
                        "the {}th record to arrive carries the sequence the journal issued it",
                        arrived + 1
                    );
                    assert_eq!(
                        event.task_id, *task,
                        "record {} arrives with the task it was appended against, so a \
                         task-level record is not silently rewritten into a queue-level one",
                        event.seq
                    );
                    assert_eq!(
                        &event.kind, kind,
                        "record {} arrives as the catalog entry that was appended, every field \
                         of it, and not as half a row",
                        event.seq
                    );
                    assert!(
                        event.ts >= before && event.ts <= after,
                        "record {} carries the instant the append stamped: {} not in \
                         {before} .. {after}",
                        event.seq,
                        event.ts
                    );
                    assert_eq!(
                        event.ts.offset(),
                        UtcOffset::UTC,
                        "record {} arrives at the UTC offset the `ts` column documents",
                        event.seq
                    );
                    arrived += 1;
                    Ok(())
                })
                .expect("three events stream");

            assert_eq!(
                arrived, 3,
                "all three records arrived, and the queue-level one with them"
            );
        }

        #[test]
        fn a_callback_that_refuses_stops_the_stream_at_the_record_it_refused() {
            let parent = scratch();
            let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            append_n(&mut journal, TEN_THOUSAND);
            let rows = stored_events(&journal.conn).len();
            let spent = counter(&journal.conn);
            let stop_at = 25u64;

            let mut visited = Visited::default();
            let error = journal
                .for_each_event(EventSeq::new(0), &mut |event| {
                    visited.visit(&event);
                    if visited.seen == stop_at {
                        return Err(reader_refused());
                    }
                    Ok(())
                })
                .expect_err("a reader that refuses the record it was handed stops the read");

            assert!(
                matches!(error, Error::Policy { .. }),
                "the caller's own refusal comes back as it was raised, not wrapped in a \
                 database failure that sends someone to look at the file: {error}"
            );
            assert!(
                error.to_string().contains("the reader stopped"),
                "and it is still the caller's message: {error}"
            );
            assert_eq!(
                visited.seen, stop_at,
                "the read stopped where its reader stopped, after {stop_at} records out of the \
                 {TEN_THOUSAND} in the journal — which is what a read that hands records over as \
                 it goes can do and one that reads everything first cannot"
            );
            assert_eq!(
                stored_events(&journal.conn).len(),
                rows,
                "a walk a reader abandoned left no row behind"
            );
            assert_eq!(
                counter(&journal.conn),
                spent,
                "and spent no sequence number, which a read has never been allowed to do"
            );
            let next = journal
                .append(Some(TaskId::new(1)), &EventKind::Resumed)
                .expect("the journal still appends after a read walked out of it");
            assert_eq!(
                next.get(),
                TEN_THOUSAND + 1,
                "which the next record's number is the proof of"
            );
        }

        #[test]
        fn a_record_that_cannot_be_decoded_stops_the_stream_naming_its_sequence() {
            let parent = scratch();
            let journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            for sequence in 1..=6i64 {
                if sequence == 4 {
                    stage_row(
                        &journal.conn,
                        sequence,
                        AN_INSTANT,
                        Some(1),
                        "PreflightStarted",
                        "{ kind: }",
                    );
                } else {
                    stage_row(
                        &journal.conn,
                        sequence,
                        AN_INSTANT,
                        Some(1),
                        "PreflightStarted",
                        r#"{"kind":"PreflightStarted"}"#,
                    );
                }
            }

            let mut visited = Visited::default();
            let error = journal
                .for_each_event(EventSeq::new(0), &mut |event| {
                    visited.visit(&event);
                    Ok(())
                })
                .expect_err("a row whose payload is not a catalog entry is not skipped past");

            assert!(
                matches!(error, Error::Corrupt { seq: Some(4), .. }),
                "the read refuses with the sequence it got far enough to name: {error}"
            );
            assert_eq!(
                visited.seen, 3,
                "the stream handed over the three records before the damaged one and then \
                 stopped, rather than skipping past it to the two sound rows behind it — a \
                 caller replaying this stream would otherwise replay a run with a hole in it"
            );
        }

        #[test]
        fn streaming_the_journal_writes_nothing_and_spends_no_sequence() {
            let parent = scratch();
            let mut journal = Journal::open(&journal_file(parent.path())).expect("a new journal");
            append_n(&mut journal, 2);

            for cursor in [0, 1, 2] {
                let mut visited = Visited::default();
                journal
                    .for_each_event(EventSeq::new(cursor), &mut |event| {
                        visited.visit(&event);
                        Ok(())
                    })
                    .expect("the same journal streams three times over");
                assert_eq!(
                    visited.seen,
                    2 - cursor,
                    "cursor {cursor} streams what is ahead of it, each of the three times"
                );
            }

            assert_eq!(
                stored_events(&journal.conn).len(),
                2,
                "three reads added no row to the table they were reading"
            );
            assert_eq!(
                counter(&journal.conn),
                2,
                "and left the sequence counter where the appends left it"
            );
            let next = journal
                .append(
                    Some(TaskId::new(1)),
                    &EventKind::Interrupted {
                        phase: Phase::Verify,
                    },
                )
                .expect("the journal still appends after being streamed");
            assert_eq!(
                next.get(),
                3,
                "which is what the next record's number proves"
            );
        }
    }
}

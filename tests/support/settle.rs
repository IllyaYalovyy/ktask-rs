//! When a screen that matches a wait is the one the stored state leads to. A TUI shows
//! what other processes stored only on its next refresh, so a screen that matched and then
//! stayed quiet may still be one refresh behind the store.
//!
//! A wait is settled when the screen has been quiet for [`QUIET`] and, if the store changed
//! since the last settled wait, a [`CYCLE`] has passed since the store or the words on
//! the screen last changed.
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    time::{Duration, Instant},
};

/// How long no frame may arrive.
pub const QUIET: Duration = Duration::from_millis(40);
/// Long enough for a refresh that started after the last change to have been drawn.
pub const CYCLE: Duration = ktask_rs_mk3::REFRESH_INTERVAL.saturating_mul(2);
/// A running worker stores its active time every second, whatever else happens. That
/// changes only durations, which no recorded screen keeps, so it is not a change here.
const HEARTBEAT: &str = "active_ms";

/// The settle rule, fed with what a wait observes.
pub struct Settle {
    store: Option<u64>,
    /// The words of the latest screen.
    words: String,
    framed: Instant,
    /// Set while a refresh is still due: when it was last made due.
    changed: Option<Instant>,
}

impl Settle {
    pub fn new(now: Instant) -> Self {
        Self {
            store: None,
            words: String::new(),
            framed: now,
            changed: None,
        }
    }

    /// The store's content as last read; `None` when there is no store.
    pub fn store(&mut self, content: Option<u64>, now: Instant) {
        if content != self.store {
            self.store = content;
            self.changed = content.map(|_| now);
        }
    }

    /// A frame arrived, or a wait starts from the screen. The same words again, as a live
    /// task redraws them with its durations ticking, do not postpone the refresh.
    pub fn frame(&mut self, screen: &str, now: Instant) {
        self.framed = now;
        let words = words(screen);
        if words != self.words {
            self.words = words;
            self.changed = self.changed.map(|_| now);
        }
    }

    /// The earliest moment the latest screen is settled, if nothing changes until then.
    pub fn due(&self) -> Instant {
        let quiet = self.framed + QUIET;
        self.changed
            .map_or(quiet, |changed| quiet.max(changed + CYCLE))
    }

    /// A wait took the latest screen as settled.
    pub fn settled(&mut self) {
        self.changed = None;
    }
}

/// The screen without figures and padding. A figure that changes for another reason than
/// time passing comes with a stored change, which is watched itself. This runs for every
/// frame, so it must stay far cheaper than normalising one.
fn words(screen: &str) -> String {
    screen
        .split(|c: char| c.is_ascii_digit() || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The stores a TUI shows: `<state home>/<instance>/ktask.sqlite3`, read as the TUI reads
/// them, by `PRAGMA data_version`. They are opened read-only, never created, and only while
/// a wait is watching.
pub struct Watch<'a> {
    homes: &'a [PathBuf],
    open: Vec<Open>,
}

struct Open {
    path: PathBuf,
    store: Connection,
    version: i64,
    content: u64,
}

impl<'a> Watch<'a> {
    pub fn new(homes: &'a [PathBuf]) -> Self {
        Self {
            homes,
            open: Vec::new(),
        }
    }

    /// What the stores hold, apart from the heartbeat; `None` while there is none.
    pub fn content(&mut self) -> Option<u64> {
        self.open_new();
        for open in &mut self.open {
            open.refresh();
        }
        let mut hash = DefaultHasher::new();
        for open in &self.open {
            (&open.path, open.content).hash(&mut hash);
        }
        (!self.open.is_empty()).then(|| hash.finish())
    }

    /// Opens the stores that exist by now; one created during a wait is picked up.
    fn open_new(&mut self) {
        let instances = self
            .homes
            .iter()
            .filter_map(|home| std::fs::read_dir(home).ok())
            .flatten()
            .flatten();
        for instance in instances {
            let path = instance.path().join("ktask.sqlite3");
            if self.open.iter().any(|open| open.path == path) || !path.is_file() {
                continue;
            }
            let Ok(store) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            else {
                continue;
            };
            // A store still being created reads as not there yet.
            let Ok(content) = content(&store) else {
                continue;
            };
            self.open.push(Open {
                path,
                store,
                version: 0,
                content,
            });
        }
    }
}

impl Open {
    fn refresh(&mut self) {
        let version = self
            .store
            .query_row("PRAGMA data_version", [], |row| row.get(0));
        // A read that fails while another process writes is repeated at the next look.
        if let Ok(version) = version
            && version != self.version
            && let Ok(content) = content(&self.store)
        {
            self.version = version;
            self.content = content;
        }
    }
}

/// A hash of every stored value except the heartbeat, read in one snapshot.
fn content(store: &Connection) -> rusqlite::Result<u64> {
    let snapshot = store.unchecked_transaction()?;
    let tables = snapshot
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut hash = DefaultHasher::new();
    for table in tables {
        table.hash(&mut hash);
        let mut all = snapshot.prepare(&format!("SELECT * FROM \"{table}\""))?;
        let kept: Vec<usize> = (0..all.column_count())
            .filter(|&column| all.column_name(column).is_ok_and(|name| name != HEARTBEAT))
            .collect();
        let mut rows = all.query([])?;
        while let Some(row) = rows.next()? {
            for &column in &kept {
                match row.get_ref(column)? {
                    ValueRef::Null => 0.hash(&mut hash),
                    ValueRef::Integer(value) => (1, value).hash(&mut hash),
                    ValueRef::Real(value) => (2, value.to_bits()).hash(&mut hash),
                    ValueRef::Text(value) => (3, value).hash(&mut hash),
                    ValueRef::Blob(value) => (4, value).hash(&mut hash),
                }
            }
        }
    }
    Ok(hash.finish())
}

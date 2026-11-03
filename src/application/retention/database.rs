//! SQLite reclamation: free pages return to the filesystem through incremental vacuum and
//! the WAL through one checkpoint per housekeeping call.
use super::admission::{admission_generation, invalidate_known_allocation};
use super::measure::entry_bytes;
use super::*;
use rusqlite::Connection;

/// `PRAGMA auto_vacuum` value of incremental mode.
const AUTO_VACUUM_INCREMENTAL: i64 = 2;
/// The copy and its WAL need about this many times the database size on the state filesystem.
const VACUUM_FREE_SPACE_FACTOR: i64 = 3;
/// Pages one incremental-vacuum step returns, bounding its write transaction.
const VACUUM_PAGES: i64 = 256;
/// How long WAL truncation may wait for readers while it blocks writers.
const TRUNCATE_WAIT: std::time::Duration = std::time::Duration::from_millis(20);
/// A WAL shrinking by less than this is the housekeeping's own page, not reclamation.
const WAL_RECLAIM_THRESHOLD: i64 = 64 * 1024;

fn sqlite_file_bytes(root: &File, name: &str) -> Result<i64> {
    Ok(entry_bytes(root, name)?.unwrap_or(0))
}

pub(super) fn sqlite_busy(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

pub(super) fn free_pages(db: &Connection) -> Result<i64> {
    Ok(db.pragma_query_value(None, "freelist_count", |r| r.get(0))?)
}

pub(super) fn incremental(db: &Connection) -> Result<bool> {
    let mode: i64 = db.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    Ok(mode == AUTO_VACUUM_INCREMENTAL)
}

/// Run a write transaction whose commit does not wait for the disk, so the write lock is
/// released before the WAL reaches stable storage. A crash may lose it: callers either
/// repeat it harmlessly or call [`sync_wal`] before acting on it.
pub(super) fn unsynced<T>(db: &Connection, write: impl FnOnce() -> Result<T>) -> Result<T> {
    db.pragma_update(None, "synchronous", "NORMAL")?;
    let committed = write();
    let restored = db.pragma_update(None, "synchronous", "FULL");
    let committed = committed?;
    restored?;
    Ok(committed)
}

/// Make every committed transaction durable without holding the write lock: commits
/// already wrote their frames, so syncing the WAL file is the commit's own fsync.
pub(super) fn sync_wal(root: &File) -> Result<()> {
    match nix::fcntl::openat(
        root,
        "ktask.sqlite3-wal",
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    ) {
        Ok(fd) => Ok(File::from(fd).sync_data()?),
        // Without a WAL every commit is already in the database file.
        Err(nix::errno::Errno::ENOENT) => Ok(()),
        Err(error) => Err(io(error)),
    }
}

/// Return one bounded step of free pages, inside the caller's write transaction or as its
/// own; a busy database keeps them for the next call.
pub(super) fn vacuum_free_pages(db: &Connection) -> Result<()> {
    if !incremental(db)? {
        return Ok(());
    }
    let vacuum = (|| -> rusqlite::Result<()> {
        let mut statement = db.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    })();
    match vacuum {
        Err(error) if !sqlite_busy(&error) => Err(error.into()),
        _ => Ok(()),
    }
}

/// Copy the WAL back without blocking writers, then truncate what is left. Truncation
/// holds the write lock while it waits for readers, so it waits only briefly; a busy
/// reader or writer keeps the WAL intact and the next call can reclaim it safely.
fn checkpoint(db: &Connection) -> Result<()> {
    db.busy_timeout(TRUNCATE_WAIT)?;
    let checkpoint = db
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get::<_, i64>(0))
        .and_then(|_| {
            db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            })
        });
    db.busy_timeout(crate::storage::BUSY_TIMEOUT)?;
    match checkpoint {
        Err(error) if !sqlite_busy(&error) => Err(error.into()),
        _ => Ok(()),
    }
}

/// The database's physical state when housekeeping began.
pub(super) struct Reclamation {
    database: i64,
    wal: i64,
    free_pages: i64,
    generation: i64,
}

impl Reclamation {
    pub(super) fn start(db: &Connection, root: &File) -> Result<Self> {
        Ok(Self {
            database: sqlite_file_bytes(root, "ktask.sqlite3")?,
            wal: sqlite_file_bytes(root, "ktask.sqlite3-wal")?,
            free_pages: free_pages(db)?,
            generation: admission_generation(db)?,
        })
    }

    /// Checkpoint once. Reclamation changes physical allocation without an admission, so
    /// the next bounded scan must replace the old projection, even if its previous cursor
    /// was already complete; a deletion meanwhile has already scheduled it. A single
    /// housekeeping WAL page is deliberately ignored to avoid remeasuring an idle instance
    /// forever.
    pub(super) fn finish(self, db: &Connection, root: &File) -> Result<()> {
        checkpoint(db)?;
        if admission_generation(db)? == self.generation
            && (sqlite_file_bytes(root, "ktask.sqlite3")? != self.database
                || self
                    .wal
                    .saturating_sub(sqlite_file_bytes(root, "ktask.sqlite3-wal")?)
                    >= WAL_RECLAIM_THRESHOLD
                || free_pages(db)? < self.free_pages)
        {
            invalidate_known_allocation(db, root)?;
        }
        Ok(())
    }
}

/// Existing databases created before incremental vacuum never shrink their file. The
/// conversion is one VACUUM, which needs room for a full copy plus its WAL and no live writer.
enum Vacuum {
    Incremental,
    Ready,
    Blocked(String),
}

/// SQLite builds the VACUUM copy in its temporary directory, chosen like its own default.
fn sqlite_temp_directory() -> Option<std::path::PathBuf> {
    ["SQLITE_TMPDIR", "TMPDIR"]
        .iter()
        .filter_map(std::env::var_os)
        .map(std::path::PathBuf::from)
        .chain(["/var/tmp", "/usr/tmp", "/tmp"].map(std::path::PathBuf::from))
        .find(|path| path.is_dir())
}

fn free_bytes(stat: &nix::sys::statvfs::Statvfs) -> i64 {
    let blocks = stat.blocks_available();
    let size = stat.fragment_size();
    i64::try_from(blocks.saturating_mul(size)).unwrap_or(i64::MAX)
}

fn vacuum_state(db: &Connection, root: &File) -> Result<Vacuum> {
    if incremental(db)? {
        return Ok(Vacuum::Incremental);
    }
    let busy: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE state='running' UNION ALL SELECT 1 FROM model_calls WHERE step_id IS NULL AND state='running' UNION ALL SELECT 1 FROM evidence_pins UNION ALL SELECT 1 FROM artifact_reservations)", [], |r| r.get(0))?;
    if busy {
        return Ok(Vacuum::Blocked("work is live in this instance".into()));
    }
    let database = sqlite_file_bytes(root, "ktask.sqlite3")?;
    let size = database.saturating_add(sqlite_file_bytes(root, "ktask.sqlite3-wal")?);
    let free = free_bytes(&nix::sys::statvfs::fstatvfs(root).map_err(io)?);
    let need = size.saturating_mul(VACUUM_FREE_SPACE_FACTOR);
    if free < need {
        return Ok(Vacuum::Blocked(format!(
            "{free} bytes free, about {need} needed for the copy and its WAL"
        )));
    }
    if let Some(temp) = sqlite_temp_directory()
        && let Ok(stat) = nix::sys::statvfs::statvfs(&temp)
        && free_bytes(&stat) < database
    {
        return Ok(Vacuum::Blocked(format!(
            "SQLite's temporary directory {} has {} bytes free, about {database} needed for the copy",
            temp.display(),
            free_bytes(&stat)
        )));
    }
    Ok(Vacuum::Ready)
}

pub(super) fn vacuum_note(db: &Connection, root: &File) -> Result<Option<String>> {
    Ok(match vacuum_state(db, root)? {
        Vacuum::Incremental => None,
        Vacuum::Ready => Some(
            "SQLite file cannot shrink (auto_vacuum off); a one-time VACUUM enables incremental vacuum at the next runner safe point".into(),
        ),
        Vacuum::Blocked(why) => Some(format!(
            "SQLite file cannot shrink (auto_vacuum off); the one-time VACUUM is deferred: {why}"
        )),
    })
}

impl Service {
    /// One-time conversion of a database created without incremental vacuum. Runs only at a
    /// runner safe point with no live work in the instance and enough free space; otherwise it
    /// is deferred and the Storage preview says why. An optional conversion never blocks work:
    /// a failed VACUUM is logged, shown in the preview and not retried by this process.
    /// Returns the bytes the file shrank by.
    pub(crate) fn enable_incremental_vacuum(&mut self) -> Result<Option<i64>> {
        if self.vacuum_failure.is_some() {
            return Ok(None);
        }
        let root = self.db.state_dir().try_clone()?;
        let db = self.db.connection();
        if !matches!(vacuum_state(db, &root)?, Vacuum::Ready) {
            return Ok(None);
        }
        let before = sqlite_file_bytes(&root, "ktask.sqlite3")?;
        let vacuum = db
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .and_then(|()| db.execute_batch("VACUUM"));
        if let Err(error) = vacuum {
            if !sqlite_busy(&error) {
                eprintln!("storage: incremental-vacuum conversion deferred: {error}");
                self.vacuum_failure = Some(error.to_string());
            }
            return Ok(None);
        }
        vacuum_free_pages(db)?;
        checkpoint(db)?;
        invalidate_known_allocation(db, &root)?;
        Ok(Some(before.saturating_sub(sqlite_file_bytes(
            &root,
            "ktask.sqlite3",
        )?)))
    }

    pub(super) fn with_vacuum_failure(&self, mut preview: Preview) -> Preview {
        if let (Some(why), Some(note)) = (&self.vacuum_failure, &mut preview.vacuum_note) {
            *note = format!(
                "SQLite file cannot shrink (auto_vacuum off); the one-time VACUUM failed and is deferred for this process: {why}"
            );
        }
        preview
    }
}

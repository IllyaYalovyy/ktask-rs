//! Physical allocation of the opened instance directory, never a path inferred from cwd
//! or HOME. One resumable walker serves both the full measurement and the bounded scan.
use super::admission::{scan_base, settle_scan};
use super::database::unsynced;
use super::*;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::Mode,
};
use rusqlite::Connection;
use serde::Deserialize;
use std::{
    ffi::OsStr,
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
};

const DIRECTORY: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
/// The instance layout is four levels deep; anything deeper is not ours to descend.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Allocation {
    pub bytes: i64,
    pub files: i64,
    pub database: i64,
    pub wal: i64,
    pub runner_logs: i64,
    pub evidence: i64,
    pub evidence_entries: i64,
    pub directories: i64,
}

fn runner_log_name(name: &[u8]) -> bool {
    name.starts_with(b"runner.log")
        || name.strip_prefix(b"runner-").is_some_and(|suffix| {
            suffix
                .strip_suffix(b".log")
                .or_else(|| suffix.strip_suffix(b".log.1"))
                .is_some_and(|id| !id.is_empty() && id.iter().all(u8::is_ascii_digit))
        })
}

/// Allocated bytes of one entry of the state root, if it exists.
pub(super) fn entry_bytes(root: &File, name: &str) -> Result<Option<i64>> {
    let path = format!("/proc/self/fd/{}/{name}", root.as_raw_fd());
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta.blocks() as i64 * 512)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn entries(root: &File, names: impl IntoIterator<Item = String>) -> Result<Allocation> {
    let mut usage = Allocation::default();
    for name in names {
        if let Some(bytes) = entry_bytes(root, &name)? {
            usage.bytes = usage.bytes.saturating_add(bytes);
            usage.files += 1;
        }
    }
    Ok(usage)
}

/// The fixed files of the instance and the root itself: a few `fstat` calls.
pub(super) fn known_allocation(root: &File) -> Result<Allocation> {
    let mut usage = entries(
        root,
        [
            "ktask.sqlite3",
            "ktask.sqlite3-wal",
            "ktask.sqlite3-shm",
            "runner.log",
            "runner.log.1",
            "artifact-publication.lock",
            "evidence",
        ]
        .map(String::from),
    )?;
    usage.bytes = usage
        .bytes
        .saturating_add(root.metadata()?.blocks() as i64 * 512);
    usage.files += 1;
    Ok(usage)
}

pub(super) fn project_runner_logs(root: &File, project: i64) -> Result<Allocation> {
    entries(
        root,
        [
            format!("runner-{project}.log"),
            format!("runner-{project}.log.1"),
        ],
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ScanFrame {
    name: Vec<u8>,
    offset: i64,
}

/// A depth-first walk that can stop after any entry and resume from its frames. Linux
/// directory offsets let a checkpoint resume without rereading older siblings. Symlinks
/// are counted as protected entries and never followed.
pub(super) struct Walk {
    frames: Vec<ScanFrame>,
    pub usage: Allocation,
    pub visited: i64,
}

impl Walk {
    fn start(root: &File) -> Result<Self> {
        Ok(Self {
            frames: vec![ScanFrame {
                name: Vec::new(),
                offset: 0,
            }],
            usage: Allocation {
                bytes: (root.metadata()?.blocks() as i64).saturating_mul(512),
                files: 1,
                directories: 1,
                ..Allocation::default()
            },
            visited: 0,
        })
    }

    fn finished(&self) -> bool {
        self.frames.is_empty()
    }

    /// Visit at most `budget` entries. A directory that vanished while the walk was paused
    /// costs one; `.`, `..` and leaving a finished directory cost nothing.
    fn step(&mut self, root: &File, mut budget: usize) -> Result<()> {
        if self.finished() {
            return Ok(());
        }
        let directory = File::from(openat(root, ".", DIRECTORY, Mode::empty()).map_err(io)?);
        self.list(0, &directory, &mut budget)?;
        Ok(())
    }

    /// Continue listing `frames[depth]`, open as `directory`, descending into each child
    /// while its own listing stays open. False when the budget ran out inside it.
    fn list(&mut self, depth: usize, directory: &File, budget: &mut usize) -> Result<bool> {
        if let Some(resumed) = self.frames.get(depth + 1) {
            let name = OsStr::from_bytes(&resumed.name);
            match openat(directory, name, DIRECTORY, Mode::empty()) {
                Ok(child) => {
                    if !self.list(depth + 1, &File::from(child), budget)? {
                        return Ok(false);
                    }
                }
                // Removed while the walk was paused; continue after it.
                Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => {
                    self.frames.truncate(depth + 1);
                    if *budget == 0 {
                        return Ok(false);
                    }
                    *budget -= 1;
                }
                Err(error) => return Err(io(error)),
            }
        }
        rustix::fs::seek(
            directory,
            rustix::fs::SeekFrom::Start(self.frames[depth].offset as u64),
        )
        .map_err(|error| Error::Io(error.into()))?;
        let mut buffer = [std::mem::MaybeUninit::uninit(); 8192];
        let mut listing = rustix::fs::RawDir::new(directory, &mut buffer);
        while *budget > 0 {
            let Some(entry) = listing.next() else {
                self.frames.pop();
                return Ok(true);
            };
            let entry = entry.map_err(|error| Error::Io(error.into()))?;
            let name = entry.file_name().to_bytes();
            self.frames[depth].offset = entry.next_entry_cookie() as i64;
            if name == b"." || name == b".." {
                continue;
            }
            self.visited += 1;
            *budget -= 1;
            if let Some(child) = self.count(directory, name)? {
                require(
                    self.frames.len() < MAX_DEPTH,
                    "Storage scan exceeded directory depth limit",
                )?;
                self.frames.push(ScanFrame {
                    name: name.to_vec(),
                    offset: 0,
                });
                if !self.list(depth + 1, &child, budget)? {
                    return Ok(false);
                }
            }
        }
        Ok(false)
    }

    /// Count one entry by its category; return the opened directory to descend into.
    fn count(&mut self, directory: &File, name: &[u8]) -> Result<Option<File>> {
        // SQLite may remove and recreate these entries behind a directory cookie.
        // Measure the fixed set only after the resumable walk finishes.
        if self.frames.len() == 1
            && matches!(
                name,
                b"ktask.sqlite3" | b"ktask.sqlite3-wal" | b"ktask.sqlite3-shm"
            )
        {
            return Ok(None);
        }
        let name = OsStr::from_bytes(name);
        let stat = match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(Error::Io(error.into())),
        };
        let child = if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
            match openat(directory, name, DIRECTORY, Mode::empty()) {
                Ok(fd) => Some(File::from(fd)),
                Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => None,
                Err(error) => return Err(io(error)),
            }
        } else {
            None
        };
        let physical = (stat.st_blocks as i64).saturating_mul(512);
        let name = name.as_bytes();
        let at_root = self.frames.len() == 1;
        let in_evidence = self
            .frames
            .get(1)
            .is_some_and(|frame| frame.name == b"evidence");
        let usage = &mut self.usage;
        usage.bytes = usage.bytes.saturating_add(physical);
        usage.files += 1;
        if child.is_some() {
            usage.directories += 1;
        }
        if at_root && child.is_none() && name == b"ktask.sqlite3" {
            usage.database += physical;
        } else if at_root && child.is_none() && name == b"ktask.sqlite3-wal" {
            usage.wal += physical;
        } else if at_root && child.is_none() && runner_log_name(name) {
            usage.runner_logs += physical;
        } else if in_evidence {
            usage.evidence += physical;
            usage.evidence_entries += 1;
        }
        Ok(child)
    }
}

pub(super) fn with_sqlite(root: &File, mut usage: Allocation) -> Result<Allocation> {
    for name in ["ktask.sqlite3", "ktask.sqlite3-wal", "ktask.sqlite3-shm"] {
        if let Some(bytes) = entry_bytes(root, name)? {
            usage.bytes = usage.bytes.saturating_add(bytes);
            usage.files += 1;
            match name {
                "ktask.sqlite3" => usage.database = bytes,
                "ktask.sqlite3-wal" => usage.wal = bytes,
                _ => {}
            }
        }
    }
    Ok(usage)
}

/// The whole instance in one walk.
pub(super) fn allocation(root: &File) -> Result<Allocation> {
    let mut walk = Walk::start(root)?;
    walk.step(root, usize::MAX)?;
    debug_assert!(walk.finished());
    with_sqlite(root, walk.usage)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScanProgress {
    Visited,
    Contended,
    Settled,
}

/// The durable scan row: 0 walking, 1 walked, 2 settled.
pub(super) struct ScanState {
    pub revision: i64,
    pub complete: i64,
    pub walk: Walk,
}

pub(super) fn scan_state(db: &Connection) -> Result<ScanState> {
    let (mut state, frames): (ScanState, String) = db.query_row(
        "SELECT revision,frames_json,bytes,files,database_bytes,wal_bytes,runner_log_bytes,evidence_bytes,evidence_entries,directory_entries,entries_visited,complete FROM instance_allocation_scan WHERE id=1",
        [], |r| Ok((ScanState {
            revision: r.get(0)?, complete: r.get(11)?,
            walk: Walk {
                frames: Vec::new(),
                usage: Allocation { bytes: r.get(2)?, files: r.get(3)?, database: r.get(4)?, wal: r.get(5)?, runner_logs: r.get(6)?, evidence: r.get(7)?, evidence_entries: r.get(8)?, directories: r.get(9)? },
                visited: r.get(10)?,
            },
        }, r.get(1)?))
    )?;
    let cursor: serde_json::Value = serde_json::from_str(&frames)
        .map_err(|e| Error::Conflict(format!("Invalid storage scan cursor: {e}")))?;
    state.walk.frames = serde_json::from_value(cursor["frames"].clone())
        .map_err(|e| Error::Conflict(format!("Invalid storage scan cursor: {e}")))?;
    Ok(state)
}

/// Start a walk once the settled one is out of date. A walk in progress is never reset:
/// what changes while it runs is applied when it settles.
fn start_scan(db: &Database) -> Result<()> {
    let current: Option<(i64, i64)> = db
        .connection()
        .query_row(
            "SELECT s.complete,a.generation-s.generation FROM instance_storage_admission a JOIN instance_allocation_scan s ON s.id=1 WHERE a.id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if current.is_some_and(|(complete, behind)| complete != 2 || behind == 0) {
        return Ok(());
    }
    let walk = Walk::start(db.state_dir())?;
    let frames = serde_json::json!({"sqlite_excluded":true,"frames":walk.frames}).to_string();
    // Scan progress is redoable: a lost commit repeats the step, and a lost settlement
    // leaves the larger projection.
    unsynced(db.connection(), || {
        let tx =
            rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
        let (generation, base_bytes, base_files) = scan_base(&tx)?;
        tx.execute("INSERT INTO instance_allocation_scan(id,generation,revision,frames_json,bytes,files,database_bytes,wal_bytes,runner_log_bytes,evidence_bytes,evidence_entries,directory_entries,entries_visited,complete,base_bytes,base_files) VALUES (1,?1,0,?2,?3,1,0,0,0,0,0,1,0,0,?4,?5) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,revision=instance_allocation_scan.revision+1,frames_json=excluded.frames_json,bytes=excluded.bytes,files=1,database_bytes=0,wal_bytes=0,runner_log_bytes=0,evidence_bytes=0,evidence_entries=0,directory_entries=1,entries_visited=0,complete=0,base_bytes=excluded.base_bytes,base_files=excluded.base_files WHERE instance_allocation_scan.complete=2 AND instance_allocation_scan.generation!=excluded.generation", params![generation,frames,walk.usage.bytes,base_bytes,base_files])?;
        tx.commit()?;
        Ok(())
    })
}

/// One bounded step of the shared allocation scan; any process may advance it.
pub(super) fn bounded_allocation_scan(db: &Database) -> Result<ScanProgress> {
    // Old cursors counted SQLite through directory cookies. Restart that ephemeral
    // measurement once when upgrading, without changing any retained evidence.
    let legacy: bool = db.connection().query_row("SELECT EXISTS(SELECT 1 FROM instance_allocation_scan WHERE coalesce(json_extract(frames_json,'$.sqlite_excluded'),0)=0)",[],|r|r.get(0))?;
    if legacy {
        db.connection().execute("DELETE FROM instance_allocation_scan WHERE coalesce(json_extract(frames_json,'$.sqlite_excluded'),0)=0", [])?;
    }
    start_scan(db)?;
    let mut state = scan_state(db.connection())?;
    if state.complete == 2 {
        return Ok(ScanProgress::Settled);
    }
    if state.complete == 0 {
        state.walk.step(db.state_dir(), CLEANUP_BATCH as usize)?;
        let walk = &state.walk;
        let complete = i64::from(walk.finished());
        let changed = unsynced(db.connection(), || {
            let tx = rusqlite::Transaction::new_unchecked(
                db.connection(),
                TransactionBehavior::Immediate,
            )?;
            let changed = tx.execute("UPDATE instance_allocation_scan SET revision=revision+1,frames_json=?1,bytes=?2,files=?3,database_bytes=?4,wal_bytes=?5,runner_log_bytes=?6,evidence_bytes=?7,evidence_entries=?8,directory_entries=?9,entries_visited=?10,complete=?11 WHERE id=1 AND revision=?12 AND complete=0", params![serde_json::json!({"sqlite_excluded":true,"frames":walk.frames}).to_string(),walk.usage.bytes,walk.usage.files,walk.usage.database,walk.usage.wal,walk.usage.runner_logs,walk.usage.evidence,walk.usage.evidence_entries,walk.usage.directories,walk.visited,complete,state.revision])?;
            tx.commit()?;
            Ok(changed)
        })?;
        if changed == 0 {
            return Ok(ScanProgress::Contended);
        }
        if complete == 0 {
            return Ok(ScanProgress::Visited);
        }
        state.revision += 1;
    }
    if settle_scan(db, state.revision, &state.walk)? {
        Ok(ScanProgress::Settled)
    } else {
        Ok(ScanProgress::Contended)
    }
}

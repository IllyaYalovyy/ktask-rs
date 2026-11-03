//! Project-scoped retention commands. The database write lock covers selection and deletion.
use super::*;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::Mode,
    unistd::{UnlinkatFlags, unlinkat},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::{
    fd::AsRawFd,
    unix::{ffi::OsStrExt, fs::MetadataExt},
};

const CLEANUP_BATCH: i64 = 64;
/// A self-hosting instance's SQLite file alone can exceed 1 GiB, so the default budget is
/// several GiB. A project may still configure any value.
const DEFAULT_MAX_BYTES: i64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_MAX_FILES: i64 = 100_000;
const REFERENCED: &str = "NOT EXISTS (SELECT 1 FROM attempt_steps source JOIN attempt_steps reused ON reused.reused_from_step_id=source.id WHERE source.attempt_id=a.id)";

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct Policy {
    pub days: i64,
    pub max_bytes: i64,
    pub max_files: i64,
    pub version: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Candidate {
    pub task_id: i64,
    pub attempt_id: i64,
    pub output_bytes: i64,
    pub output_records: i64,
    pub artifact_bytes: i64,
    pub artifacts: i64,
    pub pending_artifacts: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub policy: Policy,
    pub usage_bytes: i64,
    pub reclaimable_bytes: i64,
    /// Physical allocation for the entire instance, including SQLite and its WAL.
    pub allocated_bytes: i64,
    pub allocated_files: i64,
    pub database_bytes: i64,
    pub wal_bytes: i64,
    pub runner_log_bytes: i64,
    pub evidence_bytes: i64,
    pub evidence_entries: i64,
    pub directory_entries: i64,
    pub eligible_file_bytes: i64,
    pub reusable_database_bytes: i64,
    pub protected_bytes: i64,
    /// Closed evidence younger than the retention minimum; never cleaned.
    pub protected_by_age_bytes: i64,
    /// Closed evidence that a reused check still refers to.
    pub protected_by_reference_bytes: i64,
    /// Evidence of open, running or unresolved tasks.
    pub protected_open_bytes: i64,
    /// Present while the database has not yet been converted to incremental vacuum.
    pub vacuum_note: Option<String>,
    pub over_capacity: bool,
    pub work_capacity_reached: bool,
    pub launch_capacity_reached: bool,
    pub effective_max_bytes: i64,
    pub effective_max_files: i64,
    pub candidates: Vec<Candidate>,
    pub deferred_for_live_work: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CleanupReceipt {
    pub attempts: usize,
    pub bytes: i64,
    pub batch_limit_reached: bool,
}

fn policy(db: &Connection, project: i64) -> Result<Policy> {
    Ok(db
        .query_row(
            "SELECT r.days,coalesce(l.max_bytes,8589934592),coalesce(l.max_files,100000),r.version FROM project_retention r LEFT JOIN project_storage_limits l ON l.project_id=r.project_id WHERE r.project_id=?1",
            [project],
            |r| {
                Ok(Policy {
                    days: r.get(0)?,
                    max_bytes: r.get(1)?,
                    max_files: r.get(2)?,
                    version: r.get(3)?,
                })
            },
        )
        .optional()?
        .unwrap_or(Policy {
            days: 30,
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: DEFAULT_MAX_FILES,
            version: 0,
        }))
}

fn effective_limits(db: &Connection) -> Result<(i64, i64)> {
    Ok(db.query_row(
        "SELECT min(coalesce(l.max_bytes,8589934592)),min(coalesce(l.max_files,100000)) FROM projects p LEFT JOIN project_storage_limits l ON l.project_id=p.id",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

fn work_capacity_reached(usage: &Allocation, policy: Policy) -> bool {
    let byte_reserve = (policy.max_bytes / 4).min(256 * 1024 * 1024);
    let file_reserve = (policy.max_files / 4).clamp(1, 256);
    usage.bytes.saturating_add(byte_reserve) >= policy.max_bytes
        || usage.files.saturating_add(file_reserve) >= policy.max_files
}

fn launch_capacity_reached(usage: &Allocation, policy: Policy) -> bool {
    let byte_reserve = (policy.max_bytes / 2).min(256 * 1024 * 1024);
    let file_reserve = (policy.max_files / 2).clamp(1, 256);
    usage.bytes.saturating_add(byte_reserve) >= policy.max_bytes
        || usage.files.saturating_add(file_reserve) >= policy.max_files
}

/// Initialize the instance-wide admission watermark outside the database write lock.
/// Concurrent opens may measure the same instance; only the first inserts its row.
pub(super) fn initialize_capacity_admission(db: &Database) -> Result<()> {
    if db.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM instance_storage_admission)",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    let known = known_allocation(db.state_dir())?;
    let projects = db
        .connection()
        .prepare("SELECT id FROM projects")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let logs = projects
        .into_iter()
        .map(|id| Ok((id, project_runner_logs(db.state_dir(), id)?)))
        .collect::<Result<Vec<_>>>()?;
    let observed = allocation(db.state_dir())?;
    let tx = rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO instance_storage_admission(id,projected_bytes,projected_files,generation,known_bytes,known_files) VALUES (1,?1,?2,0,?3,?4)",
        params![observed.bytes, observed.files,known.bytes,known.files],
    )?;
    if inserted != 0 {
        for (id, log) in logs {
            tx.execute("INSERT INTO instance_runner_log_baselines(project_id,bytes,files) VALUES (?1,?2,?3)", params![id,log.bytes,log.files])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Output rows and artifact reservations claim a conservative allowance in
/// their SQLite write transaction, serialized across projects in one instance.
pub(super) fn admit_capacity(
    tx: &Connection,
    output_bytes: u64,
    output_rows: u64,
    artifact_bytes: u64,
    artifact_files: u64,
) -> Result<()> {
    let (max_bytes, max_files) = effective_limits(tx)?;
    let (projected_bytes, projected_files): (i64, i64) = tx.query_row(
        "SELECT projected_bytes,projected_files FROM instance_storage_admission WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // SQLite may allocate both database and WAL pages for a row. A published
    // artifact may also create evidence, attempt and step directories. The
    // unused quarter remains for interruption, status and recovery records.
    let claim_bytes = output_bytes
        .saturating_mul(4)
        .saturating_add(output_rows.saturating_mul(32768))
        .saturating_add(artifact_bytes.saturating_mul(4))
        .saturating_add(artifact_files.saturating_mul(16384))
        .min(i64::MAX as u64) as i64;
    let claim_files = artifact_files.saturating_mul(4).min(i64::MAX as u64) as i64;
    let byte_reserve = (max_bytes / 4).min(256 * 1024 * 1024);
    let file_reserve = (max_files / 4).clamp(1, 256);
    if projected_bytes
        .saturating_add(claim_bytes)
        .saturating_add(byte_reserve)
        >= max_bytes
        || projected_files
            .saturating_add(claim_files)
            .saturating_add(file_reserve)
            >= max_files
    {
        return Err(Error::Conflict(format!(
            "Resource limit: shared storage capacity reserve reached (projected {projected_bytes} / {max_bytes} bytes, {projected_files} / {max_files} files); accepted evidence retained; raise capacity or reclaim closed evidence before resuming"
        )));
    }
    tx.execute(
        "UPDATE instance_storage_admission SET projected_bytes=projected_bytes+?1,projected_files=projected_files+?2,generation=generation+1 WHERE id=1",
        params![claim_bytes, claim_files],
    )?;
    Ok(())
}

fn admission_generation(db: &Database) -> Result<i64> {
    Ok(db.connection().query_row(
        "SELECT generation FROM instance_storage_admission WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

pub(super) fn bump_admission_generation(db: &Connection) -> Result<()> {
    db.execute(
        "UPDATE instance_storage_admission SET generation=generation+1 WHERE id=1",
        [],
    )?;
    Ok(())
}

/// A measurement is usable only if no producer changed the claim after its
/// starting generation. The pending-reservation sum and CAS share one write
/// transaction, so a concurrent publisher cannot vanish between them.
fn reconcile_capacity_admission(
    db: &Database,
    generation: i64,
    observed: &Allocation,
) -> Result<bool> {
    let known = known_allocation(db.state_dir())?;
    let tx = rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
    if admission_generation(db)? != generation {
        return Ok(false);
    }
    // A guardian may still be copying a reserved artifact. Its bytes may not
    // exist on disk yet, so reconciliation must continue to hold that claim.
    let (pending_bytes, pending_files): (i64, i64) = tx.query_row(
        "SELECT coalesce(sum(size_bytes),0),count(*) FROM artifact_reservations",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let projected_bytes = observed
        .bytes
        .saturating_add(pending_bytes.saturating_mul(4))
        .saturating_add(pending_files.saturating_mul(16384));
    let projected_files = observed
        .files
        .saturating_add(pending_files.saturating_mul(4));
    tx.execute(
        "UPDATE instance_storage_admission SET projected_bytes=?1,projected_files=?2,known_bytes=?3,known_files=?4,generation=generation+1 WHERE id=1 AND generation=?5",
        params![projected_bytes, projected_files, known.bytes, known.files, generation],
    )?;
    tx.commit()?;
    Ok(true)
}

fn projected_allocation(db: &Database) -> Result<Allocation> {
    Ok(db.connection().query_row(
        "SELECT a.projected_bytes,a.projected_files,coalesce(s.database_bytes,0),coalesce(s.wal_bytes,0),coalesce(s.runner_log_bytes,0),coalesce(s.evidence_bytes,0),coalesce(s.evidence_entries,0),coalesce(s.directory_entries,0) FROM instance_storage_admission a LEFT JOIN instance_allocation_scan s ON s.id=1 AND s.generation=a.generation AND s.complete=2 WHERE a.id=1",
        [],
        |r| Ok(Allocation { bytes:r.get(0)?,files:r.get(1)?,database:r.get(2)?,wal:r.get(3)?,runner_logs:r.get(4)?,evidence:r.get(5)?,evidence_entries:r.get(6)?,directories:r.get(7)? }),
    )?)
}

#[derive(Clone, Copy, Default)]
struct Allocation {
    bytes: i64,
    files: i64,
    database: i64,
    wal: i64,
    runner_logs: i64,
    evidence: i64,
    evidence_entries: i64,
    directories: i64,
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

#[derive(Clone, Serialize, Deserialize)]
struct ScanFrame {
    name: Vec<u8>,
    offset: i64,
}

struct ScanState {
    generation: i64,
    revision: i64,
    frames: Vec<ScanFrame>,
    usage: Allocation,
    visited: i64,
    complete: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanProgress {
    Visited,
    Contended,
    Settled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HousekeepingProgress {
    Reclaimed,
    Sweeping,
    Reconciling,
    Contended,
    Protected,
}

impl HousekeepingProgress {
    pub(crate) fn continues(self) -> bool {
        matches!(self, Self::Reclaimed | Self::Sweeping | Self::Reconciling)
    }
}

fn known_allocation(root: &File) -> Result<Allocation> {
    let fd = root.as_raw_fd();
    let mut bytes = root.metadata()?.blocks() as i64 * 512;
    let mut files = 1_i64;
    for name in [
        "ktask.sqlite3",
        "ktask.sqlite3-wal",
        "ktask.sqlite3-shm",
        "runner.log",
        "runner.log.1",
        "artifact-publication.lock",
        "evidence",
    ] {
        let path = format!("/proc/self/fd/{fd}/{name}");
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                bytes = bytes.saturating_add(meta.blocks() as i64 * 512);
                files += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Allocation {
        bytes,
        files,
        ..Allocation::default()
    })
}

fn project_runner_logs(root: &File, project: i64) -> Result<Allocation> {
    let mut usage = Allocation::default();
    for name in [
        format!("runner-{project}.log"),
        format!("runner-{project}.log.1"),
    ] {
        let path = format!("/proc/self/fd/{}/{name}", root.as_raw_fd());
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                usage.bytes = usage.bytes.saturating_add(meta.blocks() as i64 * 512);
                usage.files += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(usage)
}

fn probe_known_allocation(db: &Database, project: i64) -> Result<()> {
    let known = known_allocation(db.state_dir())?;
    let logs = project_runner_logs(db.state_dir(), project)?;
    let tx = rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
    let (previous_bytes, previous_files): (i64, i64) = tx.query_row(
        "SELECT known_bytes,known_files FROM instance_storage_admission WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let previous_logs: Option<(i64, i64)> = tx
        .query_row(
            "SELECT bytes,files FROM instance_runner_log_baselines WHERE project_id=?1",
            [project],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (previous_log_bytes, previous_log_files) = previous_logs.unwrap_or((0, 0));
    let log_growth = logs.bytes.saturating_sub(previous_log_bytes).max(0);
    let log_files = logs.files.saturating_sub(previous_log_files).max(0);
    let gained_bytes = known.bytes.saturating_sub(previous_bytes);
    let gained_files = known.files.saturating_sub(previous_files);
    // Ignore a single housekeeping WAL page. Small changes accumulate against
    // the last observed baseline; the admission reserve covers this margin.
    if gained_bytes >= 64 * 1024 || gained_files > 0 || log_growth > 0 || log_files > 0 {
        tx.execute("UPDATE instance_storage_admission SET projected_bytes=projected_bytes+?1,projected_files=projected_files+?2,known_bytes=?3,known_files=?4,generation=generation+1 WHERE id=1", params![gained_bytes.max(0).saturating_add(log_growth),gained_files.max(0).saturating_add(log_files),known.bytes,known.files])?;
    } else if logs.bytes < previous_log_bytes || logs.files < previous_log_files {
        tx.execute("UPDATE instance_storage_admission SET known_bytes=?1,known_files=?2,generation=generation+1 WHERE id=1", params![known.bytes,known.files])?;
    } else if known.bytes < previous_bytes || known.files < previous_files {
        tx.execute(
            "UPDATE instance_storage_admission SET known_bytes=?1,known_files=?2 WHERE id=1",
            params![known.bytes, known.files],
        )?;
    }
    if previous_logs != Some((logs.bytes, logs.files)) {
        tx.execute("INSERT INTO instance_runner_log_baselines(project_id,bytes,files) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET bytes=excluded.bytes,files=excluded.files", params![project,logs.bytes,logs.files])?;
    }
    tx.commit()?;
    Ok(())
}

fn scan_state(db: &Connection) -> Result<ScanState> {
    let (mut state, frames): (ScanState, String) = db.query_row(
        "SELECT generation,revision,frames_json,bytes,files,database_bytes,wal_bytes,runner_log_bytes,evidence_bytes,evidence_entries,directory_entries,entries_visited,complete FROM instance_allocation_scan WHERE id=1",
        [], |r| Ok((ScanState {
            generation: r.get(0)?, revision: r.get(1)?, frames: Vec::new(),
            usage: Allocation { bytes: r.get(3)?, files: r.get(4)?, database: r.get(5)?, wal: r.get(6)?, runner_logs: r.get(7)?, evidence: r.get(8)?, evidence_entries: r.get(9)?, directories: r.get(10)? },
            visited: r.get(11)?, complete: r.get(12)?,
        }, r.get(2)?))
    )?;
    state.frames = serde_json::from_str(&frames)
        .map_err(|e| Error::Conflict(format!("Invalid storage scan cursor: {e}")))?;
    Ok(state)
}

/// Open one directory from the durable cursor. Missing paths were removed
/// while the scan was paused; its generation check prevents stale installation.
fn scan_directory(root: &File, frames: &[ScanFrame]) -> Result<Option<File>> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut directory = File::from(openat(root, ".", flags, Mode::empty()).map_err(io)?);
    for frame in frames.iter().skip(1) {
        match openat(
            &directory,
            std::ffi::OsStr::from_bytes(&frame.name),
            flags,
            Mode::empty(),
        ) {
            Ok(fd) => directory = File::from(fd),
            Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => return Ok(None),
            Err(error) => return Err(io(error)),
        }
    }
    Ok(Some(directory))
}

/// Linux directory offsets let each checkpoint resume without rereading its
/// older siblings. The fixed buffer and entry budget bound time and descriptors.
fn scan_entries(root: &File, state: &mut ScanState, mut budget: usize) -> Result<()> {
    while budget > 0 && !state.frames.is_empty() {
        let Some(directory) = scan_directory(root, &state.frames)? else {
            state.frames.pop();
            budget -= 1;
            continue;
        };
        let fd = directory.as_raw_fd();
        let offset = state.frames.last().unwrap().offset;
        rustix::fs::seek(&directory, rustix::fs::SeekFrom::Start(offset as u64))
            .map_err(|error| Error::Io(error.into()))?;
        let mut buffer = [std::mem::MaybeUninit::uninit(); 8192];
        let mut entries = rustix::fs::RawDir::new(&directory, &mut buffer);
        let mut descended = false;
        while budget > 0 {
            let Some(entry) = entries.next() else {
                break;
            };
            let entry = entry.map_err(|error| Error::Io(error.into()))?;
            let name = entry.file_name().to_bytes();
            state.frames.last_mut().unwrap().offset = entry.next_entry_cookie() as i64;
            state.visited += 1;
            budget -= 1;
            if name == b"." || name == b".." {
                continue;
            }
            let mut path = std::path::PathBuf::from(format!("/proc/self/fd/{fd}"));
            path.push(std::ffi::OsStr::from_bytes(name));
            let meta = match std::fs::symlink_metadata(path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let at_root = state.frames.len() == 1;
            let in_evidence = state
                .frames
                .get(1)
                .is_some_and(|frame| frame.name == b"evidence");
            let child = if meta.is_dir() {
                match openat(
                    &directory,
                    std::ffi::OsStr::from_bytes(name),
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(fd) => Some(File::from(fd)),
                    Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => None,
                    Err(error) => return Err(io(error)),
                }
            } else {
                None
            };
            let physical = child.as_ref().map_or(meta.blocks(), |file| {
                file.metadata().map(|m| m.blocks()).unwrap_or(meta.blocks())
            }) as i64
                * 512;
            state.usage.bytes = state.usage.bytes.saturating_add(physical);
            state.usage.files += 1;
            if at_root && name == b"ktask.sqlite3" {
                state.usage.database += physical;
            } else if at_root && name == b"ktask.sqlite3-wal" {
                state.usage.wal += physical;
            } else if at_root && runner_log_name(name) {
                state.usage.runner_logs += physical;
            } else if in_evidence {
                state.usage.evidence += physical;
                state.usage.evidence_entries += 1;
            }
            if child.is_some() {
                state.usage.directories += 1;
                require(
                    state.frames.len() < 64,
                    "Storage scan exceeded directory depth limit",
                )?;
                state.frames.push(ScanFrame {
                    name: name.to_vec(),
                    offset: 0,
                });
                descended = true;
                break;
            }
        }
        if !descended && budget > 0 {
            state.frames.pop();
            budget -= 1;
        }
    }
    Ok(())
}

fn bounded_allocation_scan(db: &Database) -> Result<ScanProgress> {
    let generation = admission_generation(db)?;
    let root_blocks = db.state_dir().metadata()?.blocks() as i64 * 512;
    let initial = serde_json::to_string(&vec![ScanFrame {
        name: Vec::new(),
        offset: 0,
    }])
    .expect("scan cursor");
    {
        let tx =
            rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
        if admission_generation(db)? != generation {
            return Ok(ScanProgress::Contended);
        }
        tx.execute("INSERT INTO instance_allocation_scan(id,generation,revision,frames_json,bytes,files,database_bytes,wal_bytes,runner_log_bytes,evidence_bytes,evidence_entries,directory_entries,entries_visited,complete) VALUES (1,?1,0,?2,?3,1,0,0,0,0,0,1,0,0) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,revision=instance_allocation_scan.revision+1,frames_json=excluded.frames_json,bytes=excluded.bytes,files=1,database_bytes=0,wal_bytes=0,runner_log_bytes=0,evidence_bytes=0,evidence_entries=0,directory_entries=1,entries_visited=0,complete=0 WHERE instance_allocation_scan.generation!=excluded.generation", params![generation,initial,root_blocks])?;
        tx.commit()?;
    }
    let mut state = scan_state(db.connection())?;
    if state.complete == 2 {
        return Ok(ScanProgress::Settled);
    }
    if state.complete == 0 {
        scan_entries(db.state_dir(), &mut state, CLEANUP_BATCH as usize)?;
    }
    let complete = if state.frames.is_empty() { 1 } else { 0 };
    if state.complete == 0 {
        let tx =
            rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
        if admission_generation(db)? != state.generation {
            return Ok(ScanProgress::Contended);
        }
        let changed = tx.execute("UPDATE instance_allocation_scan SET revision=revision+1,frames_json=?1,bytes=?2,files=?3,database_bytes=?4,wal_bytes=?5,runner_log_bytes=?6,evidence_bytes=?7,evidence_entries=?8,directory_entries=?9,entries_visited=?10,complete=?11 WHERE id=1 AND generation=?12 AND revision=?13 AND complete=0", params![serde_json::to_string(&state.frames).expect("scan cursor"),state.usage.bytes,state.usage.files,state.usage.database,state.usage.wal,state.usage.runner_logs,state.usage.evidence,state.usage.evidence_entries,state.usage.directories,state.visited,complete,state.generation,state.revision])?;
        tx.commit()?;
        if changed == 0 {
            return Ok(ScanProgress::Contended);
        }
    }
    if complete == 0 {
        return Ok(ScanProgress::Visited);
    }
    if !reconcile_capacity_admission(db, state.generation, &state.usage)? {
        return Ok(ScanProgress::Contended);
    }
    let next_generation = state.generation + 1;
    db.connection().execute("UPDATE instance_allocation_scan SET generation=?1,complete=2,revision=revision+1 WHERE id=1 AND generation=?2 AND complete=1", params![next_generation,state.generation])?;
    Ok(ScanProgress::Settled)
}

/// Inspect the opened instance directory, never a path inferred from cwd or HOME.
/// Symlinks are counted as protected entries and are never followed.
fn allocation(root: &File) -> Result<Allocation> {
    let root_metadata = root.metadata()?;
    let mut usage = Allocation {
        bytes: (root_metadata.blocks() as i64).saturating_mul(512),
        files: 1,
        directories: 1,
        ..Allocation::default()
    };
    visit_allocation(root, false, true, &mut usage)?;
    Ok(usage)
}

// Visit a child before opening the next sibling. The instance layout has bounded depth,
// so this holds one descriptor per ancestor instead of one per retained attempt.
fn visit_allocation(
    directory: &File,
    in_evidence: bool,
    at_root: bool,
    usage: &mut Allocation,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let path = format!("/proc/self/fd/{}", directory.as_raw_fd());
    for item in std::fs::read_dir(path)? {
        let item = item?;
        let meta = match std::fs::symlink_metadata(item.path()) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let name = item.file_name();
        let name = name.to_string_lossy();
        let child = if meta.is_dir() {
            match openat(
                directory,
                item.file_name().as_os_str(),
                flags,
                Mode::empty(),
            ) {
                Ok(fd) => Some(File::from(fd)),
                Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => None,
                Err(error) => return Err(io(error)),
            }
        } else {
            None
        };
        let physical = child.as_ref().map_or(meta.blocks(), |file| {
            file.metadata().map(|m| m.blocks()).unwrap_or(meta.blocks())
        }) as i64
            * 512;
        usage.bytes = usage.bytes.saturating_add(physical);
        usage.files += 1;
        if let Some(child) = child {
            usage.directories += 1;
            if in_evidence {
                usage.evidence_entries += 1;
                usage.evidence += physical;
            }
            visit_allocation(
                &child,
                in_evidence || (at_root && name == "evidence"),
                false,
                usage,
            )?;
        } else if at_root && name == "ktask.sqlite3" {
            usage.database += physical;
        } else if at_root && name == "ktask.sqlite3-wal" {
            usage.wal += physical;
        } else if at_root && runner_log_name(name.as_bytes()) {
            usage.runner_logs += physical;
        } else if in_evidence {
            usage.evidence += physical;
            usage.evidence_entries += 1;
        }
    }
    Ok(())
}

fn artifact_allocation(root: &File, attempt: i64, relative: &str) -> i64 {
    use std::os::fd::AsRawFd;
    let parts: Vec<_> = relative.split('/').collect();
    if !(parts.len() == 3 || parts.len() == 4)
        || parts[0] != "evidence"
        || parts[1] != format!("attempt-{attempt}")
        || !parts[2].starts_with("step-")
        || parts[2].contains("..")
        || (parts.len() == 4 && (parts[3].is_empty() || parts[3].contains("..")))
    {
        return 0;
    }
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let Ok(evidence) = openat(root, "evidence", flags, Mode::empty()) else {
        return 0;
    };
    let Ok(directory) = openat(&evidence, parts[1], flags, Mode::empty()) else {
        return 0;
    };
    let parent = if parts.len() == 4 {
        let Ok(step) = openat(&directory, parts[2], flags, Mode::empty()) else {
            return 0;
        };
        File::from(step)
    } else {
        File::from(directory)
    };
    let leaf = parts[parts.len() - 1];
    let path = format!("/proc/self/fd/{}/{}", parent.as_raw_fd(), leaf);
    std::fs::symlink_metadata(path)
        .map(|meta| (meta.blocks() as i64).saturating_mul(512))
        .unwrap_or(0)
}

fn sqlite_file_bytes(root: &File, name: &str) -> Result<i64> {
    let path = format!("/proc/self/fd/{}/{name}", root.as_raw_fd());
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.blocks() as i64 * 512),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn reclaim_database(db: &Connection, root: &File) -> Result<()> {
    let database_before = sqlite_file_bytes(root, "ktask.sqlite3")?;
    let wal_before = sqlite_file_bytes(root, "ktask.sqlite3-wal")?;
    let free_before: i64 = db.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let mode: i64 = db.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if mode == 2 {
        let vacuum = (|| -> rusqlite::Result<()> {
            let mut statement = db.prepare("PRAGMA incremental_vacuum(256)")?;
            let mut rows = statement.query([])?;
            while rows.next()?.is_some() {}
            Ok(())
        })();
        if let Err(error) = vacuum
            && !sqlite_busy(&error)
        {
            return Err(error.into());
        }
    }
    // SQLite arbitrates this with concurrent readers and writers. A busy reader
    // keeps the WAL intact; the next checkpoint can reclaim it safely.
    db.busy_timeout(std::time::Duration::from_millis(250))?;
    let checkpoint = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        r.get::<_, i64>(0)
    });
    db.busy_timeout(crate::storage::BUSY_TIMEOUT)?;
    if let Err(error) = checkpoint
        && !sqlite_busy(&error)
    {
        return Err(error.into());
    }
    let database_after = sqlite_file_bytes(root, "ktask.sqlite3")?;
    let wal_after = sqlite_file_bytes(root, "ktask.sqlite3-wal")?;
    let free_after: i64 = db.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    if database_after != database_before
        || wal_before.saturating_sub(wal_after) >= 64 * 1024
        || free_after < free_before
    {
        // Reclamation changes physical allocation without an output admission.
        // The next bounded scan must replace the old projection, even if its
        // previous cursor was already complete. A single housekeeping WAL page
        // is deliberately ignored to avoid restarting an idle scan forever.
        invalidate_known_allocation(db, root)?;
    }
    Ok(())
}

/// Logical evidence bytes the minimum age, reuse references and open work keep, so the owner
/// can see why a budget cannot be met by cleanup.
fn protection_breakdown(db: &Connection, project: i64, cutoff: i64) -> Result<(i64, i64, i64)> {
    Ok(db.query_row(
        &format!("SELECT
            coalesce(sum(CASE WHEN t.closed_at_ms IS NOT NULL AND {REFERENCED} AND a.finished_at_ms>?2 THEN b ELSE 0 END),0),
            coalesce(sum(CASE WHEN t.closed_at_ms IS NOT NULL AND NOT {REFERENCED} THEN b ELSE 0 END),0),
            coalesce(sum(CASE WHEN t.closed_at_ms IS NULL THEN b ELSE 0 END),0)
            FROM (SELECT a.*,
                coalesce((SELECT bytes FROM attempt_output_usage WHERE attempt_id=a.id),
                         (SELECT sum(length(CAST(content AS BLOB))) FROM attempt_output WHERE attempt_id=a.id),0)
                + coalesce((SELECT sum(size_bytes) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),0) AS b
                FROM attempts a WHERE a.project_id=?1) a JOIN tasks t ON t.id=a.task_id"),
        params![project, cutoff],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// Existing databases created before incremental vacuum never shrink their file. The
/// conversion is one VACUUM, which needs room for a full copy plus its WAL and no live writer.
enum Vacuum {
    Incremental,
    Ready,
    Blocked(String),
}

/// `PRAGMA auto_vacuum` value of incremental mode.
const AUTO_VACUUM_INCREMENTAL: i64 = 2;
/// The copy and its WAL need about this many times the database size on the state filesystem.
const VACUUM_FREE_SPACE_FACTOR: i64 = 3;

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
    let mode: i64 = db.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if mode == AUTO_VACUUM_INCREMENTAL {
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

fn vacuum_note(db: &Connection, root: &File) -> Result<Option<String>> {
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

fn invalidate_known_allocation(db: &Connection, root: &File) -> Result<()> {
    let known = known_allocation(root)?;
    let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    tx.execute("UPDATE instance_storage_admission SET known_bytes=?1,known_files=?2,generation=generation+1 WHERE id=1", params![known.bytes,known.files])?;
    tx.commit()?;
    Ok(())
}

fn sqlite_busy(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

#[derive(Clone, Copy)]
struct PreviewMode {
    candidate_limit: i64,
    accounting: bool,
    recover_pins: bool,
}

fn preview(
    db: &Connection,
    root: &File,
    project: i64,
    now: i64,
    allocation: &Allocation,
    mode: PreviewMode,
) -> Result<(Preview, super::evidence_pin::Recovery)> {
    let recovered = if mode.recover_pins {
        super::evidence_pin::recover(db, root)?
    } else {
        super::evidence_pin::Recovery::default()
    };
    let policy = policy(db, project)?;
    let (effective_max_bytes, effective_max_files) = effective_limits(db)?;
    let effective = Policy {
        max_bytes: effective_max_bytes,
        max_files: effective_max_files,
        ..policy
    };
    let (output, artifacts): (i64, i64) = if mode.accounting {
        (db.query_row("SELECT coalesce(sum(length(CAST(o.content AS BLOB))),0) FROM attempt_output o JOIN attempts a ON a.id=o.attempt_id WHERE a.project_id=?1", [project], |r| r.get(0))?,
        db.query_row("SELECT coalesce(sum(f.size_bytes),0) FROM artifacts f JOIN attempts a ON a.id=f.attempt_id WHERE a.project_id=?1 AND f.pruned_at_ms IS NULL", [project], |r| r.get(0))?)
    } else {
        (0, 0)
    };
    // Judges and workers can refer to earlier transcripts. Defer the whole project while any
    // attempt, project-level provider call or evidence pin is running; the write transaction
    // rechecks. Paused and waiting attempts retain their own evidence, but unrelated closed
    // attempts can still be cleaned. A call inside an attempt is live only through its attempt,
    // so a call record left behind by a crash cannot defer cleanup forever.
    let live = protected(db, project)?;
    let mut candidates = Vec::new();
    if !live {
        // The minimum age is a hard floor: capacity pressure never widens eligibility.
        let cutoff = now.saturating_sub(policy.days.saturating_mul(86_400_000));
        let mut stmt = db.prepare(&format!("SELECT a.task_id,a.id,
            coalesce((SELECT bytes FROM attempt_output_usage WHERE attempt_id=a.id),
                     (SELECT sum(length(CAST(content AS BLOB))) FROM attempt_output WHERE attempt_id=a.id),0),
            coalesce((SELECT rows FROM attempt_output_usage WHERE attempt_id=a.id),
                     (SELECT count(*) FROM attempt_output WHERE attempt_id=a.id)),
            coalesce((SELECT sum(size_bytes) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),0),
            (SELECT count(*) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),
            (SELECT count(*) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NOT NULL)
            FROM attempts a JOIN tasks t ON t.id=a.task_id
            WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND {REFERENCED}
              AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruning_requested_at_ms IS NOT NULL AND pruned_at_ms IS NULL)
                   OR EXISTS(SELECT 1 FROM output_pruning_progress WHERE attempt_id=a.id))
              AND (EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=a.id)
                   OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL))
            ORDER BY a.id LIMIT ?3"))?;
        candidates = stmt
            .query_map(params![project, cutoff, mode.candidate_limit], |r| {
                Ok(Candidate {
                    task_id: r.get(0)?,
                    attempt_id: r.get(1)?,
                    output_bytes: r.get(2)?,
                    output_records: r.get(3)?,
                    artifact_bytes: r.get(4)?,
                    artifacts: r.get(5)?,
                    pending_artifacts: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    let reclaimable_bytes = candidates
        .iter()
        .map(|c| c.output_bytes.saturating_add(c.artifact_bytes))
        .fold(0_i64, i64::saturating_add);
    let mut eligible_file_bytes = 0_i64;
    for candidate in candidates.iter().filter(|_| mode.accounting) {
        let mut files = db.prepare(
            "SELECT relative_path FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL",
        )?;
        for path in files.query_map([candidate.attempt_id], |r| r.get::<_, String>(0))? {
            let path = path?;
            eligible_file_bytes = eligible_file_bytes.saturating_add(artifact_allocation(
                root,
                candidate.attempt_id,
                &path,
            ));
        }
    }
    let free_pages: i64 = db.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let page_bytes: i64 = db.pragma_query_value(None, "page_size", |r| r.get(0))?;
    let reusable_database_bytes = free_pages
        .saturating_mul(page_bytes)
        .min(allocation.database);
    let deferred_for_live_work = live || (candidates.is_empty() && resumable(db, project)?);
    let launch_capacity_reached = launch_capacity_reached(allocation, effective);
    let (protected_by_age_bytes, protected_by_reference_bytes, protected_open_bytes) =
        if mode.accounting || launch_capacity_reached {
            protection_breakdown(
                db,
                project,
                now.saturating_sub(policy.days.saturating_mul(86_400_000)),
            )?
        } else {
            (0, 0, 0)
        };
    let vacuum_note = vacuum_note(db, root)?;
    Ok((
        Preview {
            policy,
            usage_bytes: output.saturating_add(artifacts),
            reclaimable_bytes,
            allocated_bytes: allocation.bytes,
            allocated_files: allocation.files,
            database_bytes: allocation.database,
            wal_bytes: allocation.wal,
            runner_log_bytes: allocation.runner_logs,
            evidence_bytes: allocation.evidence,
            evidence_entries: allocation.evidence_entries,
            directory_entries: allocation.directories,
            eligible_file_bytes,
            reusable_database_bytes,
            protected_bytes: allocation
                .bytes
                .saturating_sub(eligible_file_bytes)
                .saturating_sub(reusable_database_bytes),
            protected_by_age_bytes,
            protected_by_reference_bytes,
            protected_open_bytes,
            vacuum_note,
            over_capacity: allocation.bytes >= effective_max_bytes
                || allocation.files >= effective_max_files,
            work_capacity_reached: work_capacity_reached(allocation, effective),
            launch_capacity_reached,
            effective_max_bytes,
            effective_max_files,
            candidates,
            deferred_for_live_work,
        },
        recovered,
    ))
}

pub(super) fn io(error: Errno) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(error as i32))
}

fn protected(db: &Connection, project: i64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state='running' UNION ALL SELECT 1 FROM model_calls WHERE project_id=?1 AND step_id IS NULL AND state='running' UNION ALL SELECT 1 FROM evidence_pins WHERE project_id=?1)", [project], |r| r.get(0))?)
}

fn resumable(db: &Connection, project: i64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('paused','waiting'))", [project], |r| r.get(0))?)
}

/// Only generated artifact paths are eligible. Directory descriptors with
/// O_NOFOLLOW prevent a swapped symlink from redirecting deletion into a project.
fn remove_transcript(root: &File, attempt: i64, relative: &str) -> Result<()> {
    let expected_dir = format!("attempt-{attempt}");
    let parts: Vec<_> = relative.split('/').collect();
    require(
        (parts.len() == 3 || parts.len() == 4)
            && parts[0] == "evidence"
            && parts[1] == expected_dir
            && if parts.len() == 3 {
                parts[2].starts_with("step-")
                    && parts[2].ends_with(".log")
                    && parts[2][5..parts[2].len() - 4].parse::<u64>().is_ok()
            } else {
                parts[2]
                    .strip_prefix("step-")
                    .is_some_and(|n| n.parse::<u64>().is_ok())
                    && parts[3].strip_prefix("file-").is_some_and(|name| {
                        name.split_once('-').is_some_and(|(pid, index)| {
                            pid.parse::<u32>().is_ok() && index.parse::<usize>().is_ok()
                        })
                    })
            },
        "Unsafe artifact path; cleanup stopped",
    )?;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let evidence = match openat(root, "evidence", flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    let directory = match openat(&evidence, expected_dir.as_str(), flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    let (parent, filename) = if parts.len() == 4 {
        let step = match openat(&directory, parts[2], flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(io(e)),
        };
        (File::from(step), parts[3])
    } else {
        (File::from(directory), parts[2])
    };
    match unlinkat(&parent, filename, UnlinkatFlags::NoRemoveDir) {
        Ok(()) => parent.sync_all().map_err(Into::into),
        Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(io(e)),
    }
}

/// Open only generated directories through pinned descriptors. A substituted
/// non-directory is preserved and does not block later eligible attempts.
enum EvidenceDirectory {
    Missing,
    Substituted,
    Open(File, File),
}

fn evidence_directory(root: &File, attempt: i64) -> Result<EvidenceDirectory> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let evidence = match openat(root, "evidence", flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(EvidenceDirectory::Missing),
        Err(error) => return Err(io(error)),
    };
    let name = format!("attempt-{attempt}");
    let directory = match openat(&evidence, name.as_str(), flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(EvidenceDirectory::Missing),
        Err(Errno::ENOTDIR | Errno::ELOOP) => return Ok(EvidenceDirectory::Substituted),
        Err(error) => return Err(io(error)),
    };
    Ok(EvidenceDirectory::Open(evidence, directory))
}

fn remove_if_empty(parent: &File, name: &str) -> Result<bool> {
    match unlinkat(parent, name, UnlinkatFlags::RemoveDir) {
        Ok(()) => {
            parent.sync_all()?;
            Ok(true)
        }
        Err(Errno::ENOENT | Errno::ENOTEMPTY | Errno::EEXIST | Errno::ENOTDIR) => Ok(false),
        Err(error) => Err(io(error)),
    }
}

#[derive(Default)]
struct SweepCursor {
    last_attempt: i64,
    active_attempt: i64,
    last_step: i64,
    last_artifact: i64,
    dirty: bool,
    complete: bool,
    pressure: bool,
    next_eligible_at_ms: i64,
}

/// A durable cursor limits both the number of attempts and the number of rows
/// within a large attempt. Only recorded steps and artifact parents are owned.
fn sweep_empty_evidence(
    db: &Connection,
    root: &File,
    project: i64,
    budget: &mut i64,
    pressure: bool,
    rescan_complete: bool,
) -> Result<Vec<i64>> {
    require(
        !protected(db, project)?,
        "Storage or active work changed during cleanup; preview again",
    )?;
    let retention = policy(db, project)?;
    let now = now_ms();
    let retention_ms = retention.days.saturating_mul(86_400_000);
    let cutoff = now.saturating_sub(retention_ms);
    let mut cursor = db.query_row("SELECT last_attempt_id,active_attempt_id,last_step_id,last_artifact_id,dirty,complete,pressure,next_eligible_at_ms FROM evidence_sweep WHERE project_id=?1", [project], |r| Ok(SweepCursor {last_attempt:r.get(0)?,active_attempt:r.get(1)?,last_step:r.get(2)?,last_artifact:r.get(3)?,dirty:r.get(4)?,complete:r.get(5)?,pressure:r.get(6)?,next_eligible_at_ms:r.get(7)?})).optional()?.unwrap_or_default();
    if cursor.dirty || cursor.pressure != pressure {
        cursor = SweepCursor::default();
    }
    cursor.pressure = pressure;
    let age_due = cursor.next_eligible_at_ms != 0 && cursor.next_eligible_at_ms <= now;
    if cursor.complete && !rescan_complete && !age_due {
        return Ok(Vec::new());
    }
    if cursor.complete {
        cursor = SweepCursor {
            pressure,
            ..SweepCursor::default()
        };
    }
    let mut visited = Vec::new();
    while *budget > 0 {
        let attempt: Option<i64> = db.query_row(&format!("SELECT a.id FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND {REFERENCED} AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM evidence_cleanup_admissions d WHERE d.attempt_id=a.id)) AND a.id=?3 AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id)"), params![project,cutoff,cursor.active_attempt], |r| r.get(0)).optional()?;
        let attempt = if let Some(id) = attempt {
            id
        } else {
            cursor.active_attempt = 0;
            cursor.last_step = 0;
            cursor.last_artifact = 0;
            let next: Option<i64> = db.query_row(&format!("SELECT a.id FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND {REFERENCED} AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM evidence_cleanup_admissions d WHERE d.attempt_id=a.id)) AND a.id>?3 AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id) ORDER BY a.id LIMIT 1"), params![project,cutoff,cursor.last_attempt], |r| r.get(0)).optional()?;
            let Some(id) = next else {
                cursor.complete = true;
                break;
            };
            cursor.active_attempt = id;
            id
        };
        let (evidence, directory) = match evidence_directory(root, attempt)? {
            EvidenceDirectory::Open(evidence, directory) => (evidence, directory),
            state => {
                if matches!(state, EvidenceDirectory::Missing) {
                    db.execute(
                        "DELETE FROM evidence_cleanup_admissions WHERE attempt_id=?1",
                        [attempt],
                    )?;
                }
                cursor.last_attempt = attempt;
                cursor.active_attempt = 0;
                *budget -= 1;
                continue;
            }
        };
        db.execute("INSERT OR IGNORE INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,?2)", params![attempt,now_ms()])?;
        let steps = db
            .prepare(
                "SELECT id FROM attempt_steps WHERE attempt_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            )?
            .query_map(params![attempt, cursor.last_step, *budget], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for step in steps {
            remove_if_empty(&directory, &format!("step-{step}"))?;
            cursor.last_step = step;
            *budget -= 1;
        }
        if *budget == 0 {
            break;
        }
        let files = db.prepare("SELECT id,relative_path FROM artifacts WHERE attempt_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?.query_map(params![attempt,cursor.last_artifact,*budget], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, path) in files {
            let parts: Vec<_> = path.split('/').collect();
            if parts.len() == 4
                && parts[0] == "evidence"
                && parts[1] == format!("attempt-{attempt}")
                && parts[2]
                    .strip_prefix("step-")
                    .is_some_and(|n| n.parse::<u64>().is_ok())
            {
                remove_if_empty(&directory, parts[2])?;
            }
            cursor.last_artifact = id;
            *budget -= 1;
        }
        if *budget == 0 {
            break;
        }
        if remove_if_empty(&evidence, &format!("attempt-{attempt}"))? {
            visited.push(attempt);
            db.execute(
                "DELETE FROM evidence_cleanup_admissions WHERE attempt_id=?1",
                [attempt],
            )?;
        }
        cursor.last_attempt = attempt;
        cursor.active_attempt = 0;
        cursor.last_step = 0;
        cursor.last_artifact = 0;
        *budget -= 1;
    }
    if cursor.complete {
        cursor.next_eligible_at_ms = db.query_row(&format!("SELECT min(a.finished_at_ms+?2) FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND {REFERENCED} AND a.finished_at_ms>?3 AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id)"), params![project,retention_ms,cutoff], |r| r.get::<_,Option<i64>>(0))?.unwrap_or(0);
    }
    db.execute("INSERT INTO evidence_sweep(project_id,last_attempt_id,active_attempt_id,last_step_id,last_artifact_id,dirty,complete,pressure,next_eligible_at_ms) VALUES (?1,?2,?3,?4,?5,0,?6,?7,?8) ON CONFLICT(project_id) DO UPDATE SET last_attempt_id=excluded.last_attempt_id,active_attempt_id=excluded.active_attempt_id,last_step_id=excluded.last_step_id,last_artifact_id=excluded.last_artifact_id,dirty=0,complete=excluded.complete,pressure=excluded.pressure,next_eligible_at_ms=excluded.next_eligible_at_ms", params![project,cursor.last_attempt,cursor.active_attempt,cursor.last_step,cursor.last_artifact,cursor.complete,cursor.pressure,cursor.next_eligible_at_ms])?;
    Ok(visited)
}

impl Service {
    /// Two rotated segments per project fit inside one quarter of the smallest
    /// project budget, leaving the remaining allocation for evidence and recovery.
    pub(crate) fn runner_log_budget(&self) -> Result<u64> {
        let (bytes, projects): (i64, i64) = self.db.connection().query_row(
            "SELECT min(coalesce(l.max_bytes,8589934592)),count(*) FROM projects p LEFT JOIN project_storage_limits l ON l.project_id=p.id",
            [], |r| Ok((r.get(0)?, r.get(1)?))
        )?;
        Ok((bytes / projects.max(1) / 8).clamp(4096, 8 * 1024 * 1024) as u64)
    }

    /// One checkpoint performs at most four bounded batches. The next run-loop
    /// iteration or restart resumes from the durable sweep cursor.
    pub fn automatic_housekeeping(&mut self) -> Result<Preview> {
        Ok(self.automatic_housekeeping_progress(4)?.0)
    }

    pub(crate) fn automatic_housekeeping_progress(
        &mut self,
        batches: usize,
    ) -> Result<(Preview, HousekeepingProgress)> {
        self.recover_pins()?;
        probe_known_allocation(&self.db, self.project_id)?;
        let projected = projected_allocation(&self.db)?;
        let mut reclaimed = false;
        let mut sweeping = false;
        for _ in 0..batches.min(4) {
            let before = self.preview_with_allocation(CLEANUP_BATCH, false, &projected)?;
            if before.deferred_for_live_work {
                break;
            }
            let free_before: i64 =
                self.db
                    .connection()
                    .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
            let receipt = self.cleanup_storage_limited(&before, CLEANUP_BATCH, Some(&projected))?;
            reclaimed |= receipt.bytes > 0 || receipt.attempts > 0;
            sweeping |= receipt.batch_limit_reached;
            let free_pages: i64 =
                self.db
                    .connection()
                    .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
            reclaimed |= free_pages < free_before;
            let incremental: i64 =
                self.db
                    .connection()
                    .pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
            if !receipt.batch_limit_reached
                && receipt.bytes == 0
                && (free_pages == 0 || incremental != 2)
            {
                break;
            }
        }
        let scan = bounded_allocation_scan(&self.db)?;
        let after = projected_allocation(&self.db)?;
        let progress = if matches!(scan, ScanProgress::Visited) {
            HousekeepingProgress::Reconciling
        } else if matches!(scan, ScanProgress::Contended) {
            HousekeepingProgress::Contended
        } else if reclaimed {
            HousekeepingProgress::Reclaimed
        } else if sweeping {
            HousekeepingProgress::Sweeping
        } else {
            HousekeepingProgress::Protected
        };
        Ok((
            self.preview_with_allocation(CLEANUP_BATCH, false, &after)?,
            progress,
        ))
    }

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
        reclaim_database(db, &root)?;
        invalidate_known_allocation(db, &root)?;
        Ok(Some(before.saturating_sub(sqlite_file_bytes(
            &root,
            "ktask.sqlite3",
        )?)))
    }

    pub fn retention_policy(&self) -> Result<Policy> {
        policy(self.db.connection(), self.project_id)
    }

    pub fn set_retention(&mut self, expected: Policy, days: i64) -> Result<Policy> {
        self.set_storage_policy(expected, days, expected.max_bytes, expected.max_files)
    }

    pub fn set_storage_policy(
        &mut self,
        expected: Policy,
        days: i64,
        max_bytes: i64,
        max_files: i64,
    ) -> Result<Policy> {
        require(
            (0..=365000).contains(&days),
            "Retention days must be between 0 and 365000",
        )?;
        require(
            (1_048_576..=1_099_511_627_776).contains(&max_bytes),
            "Storage bytes must be between 1 MiB and 1 TiB",
        )?;
        require(
            (16..=10_000_000).contains(&max_files),
            "Storage files must be between 16 and 10000000",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            policy(&tx, self.project_id)? == expected,
            "Retention changed elsewhere; reopen and retry",
        )?;
        let next = Policy {
            days,
            max_bytes,
            max_files,
            version: expected.version + 1,
        };
        tx.execute("INSERT INTO project_retention(project_id,days,version) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET days=excluded.days,version=excluded.version", params![self.project_id, days, next.version])?;
        tx.execute("INSERT INTO project_storage_limits(project_id,max_bytes,max_files) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET max_bytes=excluded.max_bytes,max_files=excluded.max_files", params![self.project_id,max_bytes,max_files])?;
        tx.commit()?;
        Ok(next)
    }

    fn recover_pins(&self) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(
            self.db.connection(),
            TransactionBehavior::Immediate,
        )?;
        let recovered = super::evidence_pin::recover(&tx, self.db.state_dir())?;
        tx.commit()?;
        recovered.finish()
    }

    fn preview_with_allocation(
        &self,
        candidate_limit: i64,
        accounting: bool,
        observed: &Allocation,
    ) -> Result<Preview> {
        let read = rusqlite::Transaction::new_unchecked(
            self.db.connection(),
            TransactionBehavior::Deferred,
        )?;
        let (preview, _) = preview(
            &read,
            self.db.state_dir(),
            self.project_id,
            now_ms(),
            observed,
            PreviewMode {
                candidate_limit,
                accounting,
                recover_pins: false,
            },
        )?;
        read.commit()?;
        Ok(self.with_vacuum_failure(preview))
    }

    fn with_vacuum_failure(&self, mut preview: Preview) -> Preview {
        if let (Some(why), Some(note)) = (&self.vacuum_failure, &mut preview.vacuum_note) {
            *note = format!(
                "SQLite file cannot shrink (auto_vacuum off); the one-time VACUUM failed and is deferred for this process: {why}"
            );
        }
        preview
    }

    pub fn storage_preview(&self) -> Result<Preview> {
        self.recover_pins()?;
        let observed = allocation(self.db.state_dir())?;
        self.preview_with_allocation(i64::MAX, true, &observed)
    }

    pub fn cleanup_storage(&mut self, expected: &Preview) -> Result<CleanupReceipt> {
        self.cleanup_storage_limited(expected, i64::MAX, None)
    }

    fn cleanup_storage_limited(
        &mut self,
        expected: &Preview,
        candidate_limit: i64,
        observed: Option<&Allocation>,
    ) -> Result<CleanupReceipt> {
        let interactive = observed.is_none();
        let root = self.db.state_dir().try_clone()?;
        let mut budget = CLEANUP_BATCH;
        let measured = if observed.is_none() {
            Some(allocation(&root)?)
        } else {
            None
        };
        let observed = observed
            .or(measured.as_ref())
            .expect("allocation available");
        let intent = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, recovered) = preview(
            &intent,
            &root,
            self.project_id,
            now_ms(),
            observed,
            PreviewMode {
                candidate_limit,
                accounting: false,
                recover_pins: true,
            },
        )?;
        require(
            !current.deferred_for_live_work
                && current.policy == expected.policy
                && current.candidates == expected.candidates,
            "Storage changed since preview; preview again",
        )?;
        if !current.candidates.is_empty() {
            bump_admission_generation(&intent)?;
        }
        let cutoff = now_ms().saturating_sub(current.policy.days.saturating_mul(86_400_000));
        let selected = intent.prepare(&format!("SELECT f.id,f.attempt_id,f.relative_path,f.size_bytes FROM artifacts f JOIN attempts a ON a.id=f.attempt_id JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND {REFERENCED} AND f.pruned_at_ms IS NULL AND (a.finished_at_ms<=?2 OR f.pruning_requested_at_ms IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id) ORDER BY f.id LIMIT ?3"))?.query_map(params![self.project_id,cutoff,CLEANUP_BATCH], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, _, _, _) in &selected {
            intent.execute("UPDATE artifacts SET pruning_requested_at_ms=max(created_at_ms,?2) WHERE id=?1 AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NULL", params![id,now_ms()])?;
        }
        for attempt in selected
            .iter()
            .map(|(_, attempt, _, _)| *attempt)
            .collect::<std::collections::BTreeSet<_>>()
        {
            intent.execute("INSERT OR IGNORE INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,?2)", params![attempt,now_ms()])?;
            intent.execute("INSERT OR IGNORE INTO output_pruning_progress(attempt_id,size_bytes) SELECT ?1,0 WHERE EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=?1)", [attempt])?;
        }
        intent.commit()?;
        recovered.finish()?;
        // If deletion or its metadata commit is interrupted, this durable intent
        // makes the missing file an explicit, retryable cleanup operation.
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (rechecked, recovered) = preview(
            &tx,
            &root,
            self.project_id,
            now_ms(),
            observed,
            PreviewMode {
                candidate_limit,
                accounting: false,
                recover_pins: true,
            },
        )?;
        let rechecked_by_id: std::collections::BTreeMap<_, _> = rechecked
            .candidates
            .iter()
            .map(|candidate| (candidate.attempt_id, candidate))
            .collect();
        require(
            !rechecked.deferred_for_live_work
                && rechecked.policy == expected.policy
                && current.candidates.iter().all(|before| {
                    rechecked_by_id
                        .get(&before.attempt_id)
                        .is_some_and(|after| {
                            before.task_id == after.task_id
                                && before.output_bytes == after.output_bytes
                                && before.output_records == after.output_records
                                && before.artifact_bytes == after.artifact_bytes
                                && before.artifacts == after.artifacts
                        })
                }),
            "Storage or active work changed during cleanup; preview again",
        )?;
        let mut failure = None;
        let mut processed = std::collections::BTreeSet::new();
        let mut processed_bytes = 0_i64;
        for (id, attempt, path, size) in &selected {
            if let Err(error) = remove_transcript(&root, *attempt, path) {
                failure = Some(error);
                break;
            }
            tx.execute("UPDATE artifacts SET pruned_at_ms=max(created_at_ms,?2),pruning_requested_at_ms=NULL WHERE id=?1 AND pruned_at_ms IS NULL", params![id, now_ms()])?;
            processed.insert(*attempt);
            processed_bytes = processed_bytes.saturating_add(*size);
            budget -= 1;
        }
        if failure.is_none() && budget > 0 {
            let rows = tx.prepare(&format!("SELECT o.attempt_id,o.sequence,length(CAST(o.content AS BLOB)) FROM attempt_output o JOIN attempts a ON a.id=o.attempt_id JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND {REFERENCED} AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM output_pruning_progress WHERE attempt_id=a.id)) AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id) ORDER BY o.attempt_id,o.sequence LIMIT ?3"))?.query_map(params![self.project_id,cutoff,budget], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut touched = std::collections::BTreeSet::new();
            for (attempt, sequence, bytes) in rows {
                tx.execute("INSERT OR IGNORE INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,?2)", params![attempt,now_ms()])?;
                tx.execute(
                    "DELETE FROM attempt_output WHERE attempt_id=?1 AND sequence=?2",
                    params![attempt, sequence],
                )?;
                tx.execute("INSERT INTO output_pruning_progress(attempt_id,size_bytes,removed_records) VALUES (?1,?2,1) ON CONFLICT(attempt_id) DO UPDATE SET size_bytes=size_bytes+excluded.size_bytes,removed_records=removed_records+1", params![attempt,bytes])?;
                touched.insert(attempt);
                processed.insert(attempt);
                processed_bytes = processed_bytes.saturating_add(bytes);
                budget -= 1;
            }
            for attempt in touched {
                let remains: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=?1)",
                    [attempt],
                    |r| r.get(0),
                )?;
                if !remains {
                    tx.execute("INSERT OR IGNORE INTO output_pruning(attempt_id,pruned_at_ms,size_bytes) SELECT attempt_id,?2,size_bytes FROM output_pruning_progress WHERE attempt_id=?1", params![attempt,now_ms()])?;
                    tx.execute(
                        "DELETE FROM output_pruning_progress WHERE attempt_id=?1",
                        [attempt],
                    )?;
                }
            }
        }
        if !processed.is_empty() {
            bump_admission_generation(&tx)?;
        }
        tx.commit()?;
        recovered.finish()?;
        if let Some(error) = failure {
            return Err(error);
        }
        if budget > 0 {
            let tx = self
                .db
                .connection_mut()
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sweep_needed: bool = interactive || tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM evidence_sweep WHERE project_id=?1 AND complete=1 AND dirty=0 AND pressure=?2 AND (next_eligible_at_ms=0 OR next_eligible_at_ms>?3))", params![self.project_id,rechecked.launch_capacity_reached,now_ms()], |r| r.get(0))?;
            if sweep_needed {
                bump_admission_generation(&tx)?;
            }
            let visited = sweep_empty_evidence(
                &tx,
                &root,
                self.project_id,
                &mut budget,
                rechecked.launch_capacity_reached,
                interactive,
            )?;
            processed.extend(visited);
            if sweep_needed {
                bump_admission_generation(&tx)?;
            }
            tx.commit()?;
            reclaim_database(self.db.connection(), self.db.state_dir())?;
            return Ok(CleanupReceipt {
                attempts: processed.len(),
                bytes: processed_bytes,
                batch_limit_reached: budget == 0,
            });
        }
        reclaim_database(self.db.connection(), self.db.state_dir())?;
        Ok(CleanupReceipt {
            attempts: processed.len(),
            bytes: processed_bytes,
            batch_limit_reached: budget == 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn physical_accounting_identifies_per_project_runner_segments() -> Result<()> {
        let (sandbox, service, _) = closed_service()?;
        let state = sandbox.path().join("state");
        std::fs::write(
            state.join(format!("runner-{}.log", service.project_id)),
            vec![b'a'; 8192],
        )?;
        std::fs::write(
            state.join(format!("runner-{}.log.1", service.project_id)),
            vec![b'b'; 8192],
        )?;
        let preview = service.storage_preview()?;
        assert!(preview.runner_log_bytes >= 16 * 1024);
        Ok(())
    }

    #[test]
    fn alternating_projects_do_not_recharge_unchanged_runner_logs() -> Result<()> {
        let (sandbox, service, _) = closed_service()?;
        let state = sandbox.path().join("state");
        let other_root = sandbox.path().join("other-project");
        std::fs::create_dir(&other_root)?;
        let other = Service::open(
            crate::storage::Database::open_in(&state)?,
            &other_root,
            None,
        )?;
        std::fs::write(
            state.join(format!("runner-{}.log", service.project_id)),
            vec![b'a'; 8192],
        )?;
        let other_log = state.join(format!("runner-{}.log", other.project_id));
        std::fs::write(&other_log, vec![b'b'; 96 * 1024])?;
        for index in 0..300 {
            std::fs::create_dir_all(state.join(format!("evidence/held-{index}")))?;
        }
        probe_known_allocation(&service.db, service.project_id)?;
        probe_known_allocation(&other.db, other.project_id)?;
        let claimed = projected_allocation(&service.db)?;
        let generation = admission_generation(&service.db)?;
        for _ in 0..6 {
            probe_known_allocation(&service.db, service.project_id)?;
            probe_known_allocation(&other.db, other.project_id)?;
        }
        assert_eq!(admission_generation(&service.db)?, generation);
        assert_eq!(projected_allocation(&service.db)?.bytes, claimed.bytes);
        let concurrent = std::thread::spawn(move || -> Result<Service> {
            for _ in 0..40 {
                probe_known_allocation(&other.db, other.project_id)?;
                std::thread::yield_now();
            }
            Ok(other)
        });
        // Each scan step also grows the WAL, and a concurrent probe legitimately charges a
        // growth of 64 KiB or more, which restarts the scan. The probing thread is finite, so
        // the scan must settle once it ends; unchanged runner logs never add a charge.
        let mut settled = false;
        let mut after_probes = 0;
        while !settled && after_probes < 30 {
            settled = bounded_allocation_scan(&service.db)? == ScanProgress::Settled;
            if concurrent.is_finished() {
                after_probes += 1;
            }
        }
        let other = concurrent.join().expect("runner probe panicked")?;
        assert!(settled, "unchanged logs kept invalidating the bounded scan");
        std::fs::rename(
            &other_log,
            state.join(format!("runner-{}.log.1", other.project_id)),
        )?;
        std::fs::write(&other_log, vec![b'c'; 4096])?;
        probe_known_allocation(&other.db, other.project_id)?;
        let rotated = projected_allocation(&other.db)?.bytes;
        assert!(rotated >= claimed.bytes);
        drop(other);
        let reopened = Service::open(
            crate::storage::Database::open_in(&state)?,
            &other_root,
            None,
        )?;
        probe_known_allocation(&reopened.db, reopened.project_id)?;
        assert_eq!(projected_allocation(&reopened.db)?.bytes, rotated);
        Ok(())
    }

    #[test]
    fn completed_allocation_scan_remeasures_successive_vacuum_batches() -> Result<()> {
        let (_sandbox, mut service, task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "vacuum",
            "fixture-base",
            false,
        )?;
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        service.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,?2)", params![run.attempt,"x".repeat(4 * 1024 * 1024)])?;
        service
            .db
            .connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        service.db.connection().execute(
            "DELETE FROM attempt_output WHERE attempt_id=?1",
            [run.attempt],
        )?;
        service
            .db
            .connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let initial = allocation(service.db.state_dir())?;
        let policy = service.retention_policy()?;
        service.set_storage_policy(
            policy,
            policy.days,
            (initial.bytes * 3 / 2).max(1_048_576),
            policy.max_files,
        )?;
        let free: i64 =
            service
                .db
                .connection()
                .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
        assert!(
            free > 512,
            "fixture did not leave multiple vacuum batches: {free}"
        );
        for _ in 0..8 {
            if bounded_allocation_scan(&service.db)? == ScanProgress::Settled {
                break;
            }
        }
        assert_eq!(scan_state(service.db.connection())?.complete, 2);
        assert!(
            service
                .preview_with_allocation(CLEANUP_BATCH, false, &projected_allocation(&service.db)?)?
                .launch_capacity_reached
        );
        let mut recovered = false;
        for _ in 0..20 {
            let (preview, _) = service.automatic_housekeeping_progress(1)?;
            if !preview.launch_capacity_reached {
                recovered = true;
                break;
            }
        }
        let physical = allocation(service.db.state_dir())?;
        assert!(
            recovered,
            "vacuum freed bytes but cached capacity remained blocked: physical={} projected={}",
            physical.bytes,
            projected_allocation(&service.db)?.bytes
        );
        assert!(physical.bytes < initial.bytes);
        Ok(())
    }

    #[test]
    fn database_without_incremental_vacuum_converts_once_at_a_safe_point() -> Result<()> {
        let (_sandbox, mut service, _) = closed_service()?;
        let db = service.db.connection();
        db.execute_batch("PRAGMA auto_vacuum=NONE; VACUUM;")?;
        let mode = |db: &Connection| -> rusqlite::Result<i64> {
            db.pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        };
        assert_eq!(mode(db)?, 0);
        let note = service
            .storage_preview()?
            .vacuum_note
            .expect("limitation is reported");
        assert!(note.contains("auto_vacuum off"), "{note}");
        // Live work in the instance defers the conversion.
        service.db.connection().execute(
            "INSERT INTO evidence_pins(id,project_id,directory_identity,owner_identity,guardian_identity) VALUES ('00000000000000000000000000000001',?1,'d','o','g')",
            [service.project_id],
        )?;
        assert_eq!(service.enable_incremental_vacuum()?, None);
        assert_eq!(mode(service.db.connection())?, 0);
        service
            .db
            .connection()
            .execute("DELETE FROM evidence_pins", [])?;
        assert!(service.enable_incremental_vacuum()?.is_some());
        assert_eq!(mode(service.db.connection())?, 2);
        assert!(service.storage_preview()?.vacuum_note.is_none());
        assert_eq!(service.enable_incremental_vacuum()?, None);
        Ok(())
    }

    #[test]
    fn failed_vacuum_conversion_is_deferred_reported_and_never_an_error() -> Result<()> {
        let (sandbox, mut service, _) = closed_service()?;
        service
            .db
            .connection()
            .execute_batch("PRAGMA auto_vacuum=NONE; VACUUM;")?;
        // VACUUM cannot run inside a transaction: a deterministic non-busy failure.
        service.db.connection().execute_batch("BEGIN")?;
        assert_eq!(service.enable_incremental_vacuum()?, None);
        service.db.connection().execute_batch("ROLLBACK")?;
        let note = service.storage_preview()?.vacuum_note.expect("reported");
        assert!(
            note.contains("failed and is deferred") && note.contains("transaction"),
            "{note}"
        );
        // This process does not retry, even once the obstacle is gone.
        assert_eq!(service.enable_incremental_vacuum()?, None);
        let mode: i64 = service
            .db
            .connection()
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
        assert_eq!(mode, 0);
        drop(service);
        let mut next = Service::open(
            crate::storage::Database::open_in(&sandbox.path().join("state"))?,
            &sandbox.path().join("project"),
            None,
        )?;
        assert!(next.enable_incremental_vacuum()?.is_some());
        assert!(next.storage_preview()?.vacuum_note.is_none());
        Ok(())
    }

    #[test]
    fn admission_reconciliation_rejects_output_publication_and_competing_scans() -> Result<()> {
        let (_sandbox, service, _) = closed_service()?;
        let generation = admission_generation(&service.db)?;
        let measured = allocation(service.db.state_dir())?;
        let tx =
            Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
        admit_capacity(&tx, 8192, 1, 0, 0)?;
        tx.commit()?;
        let claimed = projected_allocation(&service.db)?;
        assert!(!reconcile_capacity_admission(
            &service.db,
            generation,
            &measured
        )?);
        assert_eq!(projected_allocation(&service.db)?.bytes, claimed.bytes);
        let next = admission_generation(&service.db)?;
        let measured = allocation(service.db.state_dir())?;
        assert!(reconcile_capacity_admission(&service.db, next, &measured)?);
        assert!(!reconcile_capacity_admission(&service.db, next, &measured)?);
        Ok(())
    }

    #[test]
    fn completed_artifact_invalidates_measurement_before_releasing_reservation() -> Result<()> {
        let (_sandbox, mut service, task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let attempt = service
            .begin_run(
                VersionedTask { id: task, version },
                "artifact-transition",
                "fixture-base",
                false,
            )?
            .attempt;
        let relative = format!("evidence/attempt-{attempt}/step-1/file-1-0");
        let tx =
            Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
        admit_capacity(&tx, 0, 0, 8192, 1)?;
        tx.execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,'proof','text/plain',?2,8192,?3,?4,1)", params![attempt,relative,"a".repeat(64),"b".repeat(40)])?;
        tx.commit()?;
        let generation = admission_generation(&service.db)?;
        let measured = allocation(service.db.state_dir())?;
        service.finish_reservation(&relative)?;
        let claimed = projected_allocation(&service.db)?;
        assert!(!reconcile_capacity_admission(
            &service.db,
            generation,
            &measured
        )?);
        assert_eq!(projected_allocation(&service.db)?.bytes, claimed.bytes);
        let stored: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM artifacts WHERE relative_path=?1",
            [relative],
            |r| r.get(0),
        )?;
        assert_eq!(stored, 1);
        Ok(())
    }

    #[test]
    fn bounded_allocation_cursor_survives_concurrent_output_and_restart() -> Result<()> {
        let (sandbox, mut service, task) = closed_service()?;
        let evidence = sandbox.path().join("state/evidence");
        for index in 0..1200 {
            std::fs::create_dir_all(evidence.join(format!("held-{index}")))?;
        }
        let other_root = sandbox.path().join("other-project");
        std::fs::create_dir(&other_root)?;
        let state = sandbox.path().join("state");
        let mut other = Service::open(
            crate::storage::Database::open_in(&state)?,
            &other_root,
            None,
        )?;
        let definition = service.task(task)?.unwrap().definition().definition.clone();
        let other_task = other.create(&[definition], None, true)?[0];
        let version = other.current_task()?.unwrap().version;
        let run = other.begin_run(
            VersionedTask {
                id: other_task,
                version,
            },
            "scan-output",
            "fixture-base",
            false,
        )?;
        let delivery = std::thread::spawn(move || -> Result<u128> {
            let mut longest = 0;
            for _ in 0..32 {
                let started = std::time::Instant::now();
                other.output(&run, "stdout", "concurrent output")?;
                longest = longest.max(started.elapsed().as_millis());
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Ok(longest)
        });
        let mut calls = 0;
        let mut previous_visited = 0;
        for _ in 0..8 {
            let _ = bounded_allocation_scan(&service.db)?;
            calls += 1;
            // Concurrent output can contend the very first call before it records a scan.
            let visited: i64 = service
                .db
                .connection()
                .query_row(
                    "SELECT entries_visited FROM instance_allocation_scan WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            assert!(visited.saturating_sub(previous_visited) <= CLEANUP_BATCH);
            previous_visited = visited;
        }
        drop(service);
        let longest = delivery.join().expect("output thread panicked")?;
        let service = Service::open(
            crate::storage::Database::open_in(&state)?,
            &sandbox.path().join("project"),
            None,
        )?;
        loop {
            let progress = bounded_allocation_scan(&service.db)?;
            calls += 1;
            // Concurrent output can contend the very first call before it records a scan.
            let visited: i64 = service
                .db
                .connection()
                .query_row(
                    "SELECT entries_visited FROM instance_allocation_scan WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            assert!(visited.saturating_sub(previous_visited) <= CLEANUP_BATCH);
            previous_visited = visited;
            if progress == ScanProgress::Settled {
                break;
            }
            assert!(calls < 100, "bounded scan did not settle");
        }
        let scan = scan_state(service.db.connection())?;
        let physical = allocation(service.db.state_dir())?;
        assert_eq!(scan.usage.files, physical.files);
        assert!(scan.visited > 1200);
        assert!(longest < 5000, "concurrent output blocked {longest} ms");
        println!(
            "bounded allocation visited={} checkpoints={calls} longest_output_commit_ms={longest}",
            scan.visited
        );
        Ok(())
    }

    #[test]
    fn capacity_cleanup_keeps_cross_attempt_check_proof_and_counts_allocation() -> Result<()> {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        let source = fixture
            .first_directory
            .join(format!("step-{}.log", fixture.first_step));
        std::fs::write(&source, "required check transcript")?;
        service.db.connection().execute(
            "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'check','text/plain',?2,25,1)",
            params![fixture.first_attempt, format!("evidence/attempt-{}/step-{}.log", fixture.first_attempt, fixture.first_step)],
        )?;
        service.db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'source output')",
            [fixture.first_attempt],
        )?;
        service.db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'eligible output')",
            [fixture.later_attempt],
        )?;
        let (task, revision): (i64, i64) = service.db.connection().query_row(
            "SELECT task_id,revision FROM attempts WHERE id=?1",
            [fixture.later_attempt],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        service.db.connection().execute(
            "INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,reused_from_step_id,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,81,'check','succeeded','true','.',1000,?5,1,2)",
            params![fixture.later_attempt,service.project_id,task,revision,fixture.first_step],
        )?;
        let policy = service.retention_policy()?;
        service.set_storage_policy(policy, 0, 1_048_576, 10_000)?;
        let large_log = fixture.state.join("runner.log");
        std::fs::write(&large_log, vec![1_u8; 2 * 1024 * 1024])?;
        let before = service.storage_preview()?;
        assert!(before.over_capacity);
        assert!(before.runner_log_bytes >= 2 * 1024 * 1024);
        assert!(before.database_bytes > 0);
        assert!(before.directory_entries >= 80);
        assert_eq!(before.candidates.len(), 1);
        assert_eq!(before.candidates[0].attempt_id, fixture.later_attempt);
        let after = service.automatic_housekeeping()?;
        assert!(
            after.over_capacity,
            "protected allocation must remain visible"
        );
        assert!(source.exists());
        assert_eq!(
            std::fs::read_to_string(&source)?,
            "required check transcript"
        );
        let retained: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(retained, 1);
        Ok(())
    }

    #[test]
    fn automatic_housekeeping_drains_large_closed_history_and_empty_directories() -> Result<()> {
        let (sandbox, mut service, first) = closed_service()?;
        let definition = Definition {
            title: "History".into(),
            goal: "Bound retained evidence".into(),
            kind: Kind::Ai,
            criteria: vec!["Done".into()],
            models: std::array::from_fn(|_| {
                Some(ModelConfig {
                    provider: "claude".into(),
                    model: "sonnet".into(),
                    effort: None,
                })
            }),
            attempt_limit: Some(1),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        let mut tasks = vec![first];
        tasks.extend(service.create(&vec![definition; 204], None, true)?);
        let state = sandbox.path().join("state");
        let noisy_output = "x".repeat(32 * 1024);
        for task in tasks {
            let version = service.current_task()?.unwrap().version;
            let run = service.begin_run(
                VersionedTask { id: task, version },
                &format!("history-{task}"),
                "fixture-base",
                false,
            )?;
            service.output(&run, "system", &noisy_output)?;
            service.finish_run(&run, "succeeded", None, "Done", None)?;
            let directory = state.join(format!("evidence/attempt-{}", run.attempt));
            std::fs::create_dir_all(&directory)?;
            let relative = format!("evidence/attempt-{}/step-999.log", run.attempt);
            std::fs::write(state.join(&relative), "proof")?;
            service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'proof','text/plain',?2,5,1)", params![run.attempt,relative])?;
        }
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let before = service.storage_preview()?;
        assert_eq!(before.candidates.len(), 205);
        assert!(before.allocated_bytes > 6 * 1024 * 1024);
        let other_root = sandbox.path().join("other-project");
        std::fs::create_dir(&other_root)?;
        let mut other = Service::open(
            crate::storage::Database::open_in(&state)?,
            &other_root,
            None,
        )?;
        let other_definition = service
            .task(first)?
            .unwrap()
            .definition()
            .definition
            .clone();
        let other_task = other.create(&[other_definition], None, true)?[0];
        let version = other.current_task()?.unwrap().version;
        let other_run = other.begin_run(
            VersionedTask {
                id: other_task,
                version,
            },
            "concurrent-output",
            "fixture-base",
            false,
        )?;
        let other_attempt = other_run.attempt;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let delivery = std::thread::spawn(move || -> Result<(Service, u128)> {
            ready.wait();
            let mut longest_ms = 0;
            for _ in 0..32 {
                let started = std::time::Instant::now();
                other.output(&other_run, "stdout", &"live".repeat(128))?;
                longest_ms = longest_ms.max(started.elapsed().as_millis());
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Ok((other, longest_ms))
        });
        barrier.wait();
        for _ in 0..16 {
            let after = service.automatic_housekeeping()?;
            if after.candidates.is_empty()
                && after.evidence_bytes == 0
                && after.reusable_database_bytes == 0
            {
                break;
            }
        }
        let (other, longest_ms) = delivery.join().expect("output thread panicked")?;
        let delivered: i64 = other.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [other_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(delivered, 32);
        println!("concurrent project output rows={delivered} longest_commit_ms={longest_ms}");
        assert!(
            longest_ms < 5000,
            "other project's output blocked for {longest_ms} ms"
        );
        let after = service.storage_preview()?;
        let mode: i64 = service
            .db
            .connection()
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
        let pages: i64 = service
            .db
            .connection()
            .pragma_query_value(None, "page_count", |r| r.get(0))?;
        let free: i64 =
            service
                .db
                .connection()
                .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
        println!(
            "allocation before={} db={} wal={}; after={} db={} wal={}; mode={mode} pages={pages} free={free}",
            before.allocated_bytes,
            before.database_bytes,
            before.wal_bytes,
            after.allocated_bytes,
            after.database_bytes,
            after.wal_bytes
        );
        assert!(after.candidates.is_empty());
        assert_eq!(after.evidence_bytes, 0);
        assert!(after.allocated_files < before.allocated_files);
        assert!(
            after.database_bytes < 4 * 1024 * 1024,
            "SQLite allocation did not return after output pruning: {} bytes",
            after.database_bytes
        );
        let history: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM tasks WHERE project_id=?1 AND closed_at_ms IS NOT NULL",
            [service.project_id],
            |r| r.get(0),
        )?;
        assert_eq!(history, 205);
        Ok(())
    }

    #[test]
    fn paused_and_waiting_attempts_keep_output_while_closed_history_is_cleaned() -> Result<()> {
        for state in ["paused", "waiting"] {
            let fixture = sweep_fixture()?;
            let mut service = fixture.service;
            let definition = Definition {
                title: "Resumable".into(),
                goal: "Keep current evidence".into(),
                kind: Kind::Ai,
                criteria: vec!["Done".into()],
                models: std::array::from_fn(|_| {
                    Some(ModelConfig {
                        provider: "claude".into(),
                        model: "sonnet".into(),
                        effort: None,
                    })
                }),
                attempt_limit: Some(2),
                timeout_ms: None,
                stage_timeout_ms: None,
            };
            let task = service.create(&[definition], None, true)?[0];
            let version = service.current_task()?.unwrap().version;
            let run = service.begin_run(
                VersionedTask { id: task, version },
                &format!("resumable-{state}"),
                "fixture-base",
                false,
            )?;
            service.output(&run, "system", "keep current proof")?;
            service.finish_run(&run, state, None, "Waiting for continuation", None)?;
            service.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'closed proof')", [fixture.first_attempt])?;
            let preview = service.storage_preview()?;
            assert!(!preview.deferred_for_live_work, "{state}");
            assert!(
                preview
                    .candidates
                    .iter()
                    .all(|c| c.attempt_id != run.attempt)
            );
            assert!(
                preview
                    .candidates
                    .iter()
                    .any(|c| c.attempt_id == fixture.first_attempt)
            );
            service.cleanup_storage(&preview)?;
            let retained: i64 = service.db.connection().query_row(
                "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
                [run.attempt],
                |r| r.get(0),
            )?;
            assert_eq!(retained, 1, "{state}");
        }
        Ok(())
    }

    #[test]
    fn shared_admission_stops_live_output_and_artifacts_with_recovery_headroom() -> Result<()> {
        let (sandbox, mut first, task) = closed_service()?;
        let second_root = sandbox.path().join("second-project");
        std::fs::create_dir(&second_root)?;
        let state = sandbox.path().join("state");
        let mut second = Service::open(
            crate::storage::Database::open_in(&state)?,
            &second_root,
            None,
        )?;
        let definition = first.task(task)?.unwrap().definition().definition.clone();
        let second_task = second.create(&[definition], None, true)?[0];
        let observed = first.storage_preview()?;
        let limit = (observed.allocated_bytes + 1024 * 1024).max(1_048_576);
        let policy = first.retention_policy()?;
        first.set_storage_policy(policy, 30, limit, 10_000)?;
        let other_preview = second.storage_preview()?;
        assert_eq!(other_preview.effective_max_bytes, limit);
        assert_eq!(other_preview.policy.max_bytes, DEFAULT_MAX_BYTES);
        let version = first.current_task()?.unwrap().version;
        let run_first = first.begin_run(
            VersionedTask { id: task, version },
            "shared-first",
            "fixture-base",
            false,
        )?;
        let version = second.current_task()?.unwrap().version;
        let run_second = second.begin_run(
            VersionedTask {
                id: second_task,
                version,
            },
            "shared-second",
            "fixture-base",
            false,
        )?;
        let chunk = "x".repeat(16 * 1024);
        let mut accepted = 0;
        let mut denied = false;
        for _ in 0..100 {
            let (service, run) = if accepted % 2 == 0 {
                (&first, &run_first)
            } else {
                (&second, &run_second)
            };
            match service.output(run, "stdout", &chunk) {
                Ok(()) => accepted += 1,
                Err(Error::Conflict(reason))
                    if reason.starts_with("Resource limit: shared storage") =>
                {
                    denied = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        assert!(
            denied && accepted >= 2,
            "both projects should share a finite allowance"
        );
        let source = sandbox.path().join("proof.bin");
        std::fs::write(&source, vec![b'p'; 128 * 1024])?;
        let relative = format!("evidence/attempt-{}/step-999/file-1-0", run_second.attempt);
        assert!(
            matches!(second.publish_artifact(run_second.attempt, &run_second.token, "proof.bin", &source, &relative, &"a".repeat(40)), Err(Error::Conflict(reason)) if reason.starts_with("Resource limit: shared storage"))
        );
        assert!(
            !state.join(&relative).exists(),
            "denied publication created evidence"
        );
        assert!(
            first.storage_preview()?.allocated_bytes < limit,
            "recovery reserve was consumed"
        );
        let first_rows: i64 = first.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [run_first.attempt],
            |r| r.get(0),
        )?;
        let second_rows: i64 = second.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [run_second.attempt],
            |r| r.get(0),
        )?;
        assert!(first_rows > 0 && second_rows > 0);
        Ok(())
    }

    #[test]
    fn shared_file_budget_rejects_publication_before_creating_more_directories() -> Result<()> {
        let (sandbox, mut service, task) = closed_service()?;
        let observed = service.storage_preview()?;
        let limit = (observed.allocated_files + 12).max(16);
        let policy = service.retention_policy()?;
        service.set_storage_policy(policy, 30, policy.max_bytes, limit)?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "file-budget",
            "fixture-base",
            false,
        )?;
        let source = sandbox.path().join("empty-proof");
        std::fs::write(&source, b"")?;
        let mut accepted = 0;
        let mut rejected = None;
        for index in 0..100 {
            let relative = format!("evidence/attempt-{}/step-999/file-1-{index}", run.attempt);
            match service.publish_artifact(
                run.attempt,
                &run.token,
                &format!("proof-{index}"),
                &source,
                &relative,
                &"a".repeat(40),
            ) {
                Ok(()) => accepted += 1,
                Err(Error::Conflict(reason))
                    if reason.starts_with("Resource limit: shared storage") =>
                {
                    rejected = Some(relative);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        assert!(accepted > 0 && rejected.is_some());
        assert!(
            !sandbox
                .path()
                .join("state")
                .join(rejected.unwrap())
                .exists()
        );
        assert!(service.storage_preview()?.allocated_files < limit);
        Ok(())
    }

    #[test]
    fn cleanup_reclaims_after_a_wal_reader_releases_its_snapshot() -> Result<()> {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        let first_task: i64 = service.db.connection().query_row(
            "SELECT task_id FROM attempts WHERE id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        let other_state = fixture._sandbox.path().join("other-instance");
        let mut other = Service::open(
            crate::storage::Database::open_in(&other_state)?,
            &fixture.project,
            None,
        )?;
        let other_definition = service
            .task(first_task)?
            .unwrap()
            .definition()
            .definition
            .clone();
        let other_task = other.create(&[other_definition], None, true)?[0];
        let other_identity = std::fs::metadata(other_state.join("ktask.sqlite3"))?;
        let database = fixture.state.join("ktask.sqlite3");
        let identity = std::fs::metadata(&database)?;
        service.db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,?2)",
            params![fixture.first_attempt, "x".repeat(512 * 1024)],
        )?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let reader = crate::storage::Database::open_in(&fixture.state)?;
        reader.connection().execute_batch("BEGIN")?;
        let visible: i64 = reader.connection().query_row(
            "SELECT length(content) FROM attempt_output WHERE attempt_id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(visible, 512 * 1024);
        service.automatic_housekeeping()?;
        let still_visible: i64 = reader.connection().query_row(
            "SELECT length(content) FROM attempt_output WHERE attempt_id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(
            still_visible, visible,
            "reader snapshot changed during cleanup"
        );
        let retained: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(retained, 0);
        let pinned_wal = service.storage_preview()?.wal_bytes;
        assert!(
            pinned_wal > 64 * 1024,
            "reader did not pin a substantial WAL: {pinned_wal}"
        );
        reader.connection().execute_batch("COMMIT")?;
        service.automatic_housekeeping()?;
        let after = std::fs::metadata(&database)?;
        assert_eq!((identity.dev(), identity.ino()), (after.dev(), after.ino()));
        let history: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempts WHERE id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(history, 1);
        assert!(other.task(other_task)?.is_some());
        let after_other = std::fs::metadata(other_state.join("ktask.sqlite3"))?;
        assert_eq!(
            (other_identity.dev(), other_identity.ino()),
            (after_other.dev(), after_other.ino())
        );
        let reclaimed_wal = service.storage_preview()?.wal_bytes;
        assert!(
            reclaimed_wal < pinned_wal && reclaimed_wal <= 64 * 1024,
            "WAL was not reclaimed after reader release: {pinned_wal} -> {reclaimed_wal}"
        );
        Ok(())
    }

    #[test]
    fn cleanup_waits_for_a_contending_writer_without_replacing_the_database() -> Result<()> {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        let database = fixture.state.join("ktask.sqlite3");
        let identity = std::fs::metadata(&database)?;
        service.db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'writer contention')",
            [fixture.first_attempt],
        )?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let writer = crate::storage::Database::open_in(&fixture.state)?;
        writer.connection().execute_batch("BEGIN IMMEDIATE")?;
        let cleanup = std::thread::spawn(move || -> Result<(Service, Preview)> {
            let preview = service.automatic_housekeeping()?;
            Ok((service, preview))
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        writer.connection().execute_batch("COMMIT")?;
        let (service, _) = cleanup.join().expect("cleanup thread panicked")?;
        let after = std::fs::metadata(&database)?;
        assert_eq!((identity.dev(), identity.ino()), (after.dev(), after.ino()));
        let retained: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [fixture.first_attempt],
            |r| r.get(0),
        )?;
        assert_eq!(retained, 0);
        Ok(())
    }

    fn closed_service() -> Result<(crate::scratch::Scratch, Service, i64)> {
        let sandbox =
            crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let project = sandbox.path().join("project");
        std::fs::create_dir(&project)?;
        let state = sandbox.path().join("state");
        let mut service =
            Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
        let models = std::array::from_fn(|_| {
            Some(ModelConfig {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: None,
            })
        });
        let task = service.create(
            &[Definition {
                title: "Closed".into(),
                goal: "Test cleanup".into(),
                kind: Kind::Ai,
                criteria: vec!["Done".into()],
                models,
                attempt_limit: Some(1),
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        Ok((sandbox, service, task))
    }

    struct SweepFixture {
        service: Service,
        first_attempt: i64,
        first_step: i64,
        later_attempt: i64,
        first_directory: PathBuf,
        later_directory: PathBuf,
        state: PathBuf,
        project: PathBuf,
        _sandbox: crate::scratch::Scratch,
    }

    fn sweep_fixture() -> Result<SweepFixture> {
        let (sandbox, mut service, first_task) = closed_service()?;
        let project = sandbox.path().join("project");
        let state = sandbox.path().join("state");
        let version = service.current_task()?.unwrap().version;
        let first = service.begin_run(
            VersionedTask {
                id: first_task,
                version,
            },
            "earlier-failure",
            "fixture-base",
            false,
        )?;
        service.finish_run(&first, "failed", Some("check"), "Failed", None)?;
        let version = service.current_task()?.unwrap().version;
        service.mark_done(
            VersionedTask {
                id: first_task,
                version,
            },
            "Resolved",
        )?;
        let models = std::array::from_fn(|_| {
            Some(ModelConfig {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: None,
            })
        });
        let later_task = service.create(
            &[Definition {
                title: "Later".into(),
                goal: "Sweep".into(),
                kind: Kind::Ai,
                criteria: vec!["Done".into()],
                models,
                attempt_limit: Some(1),
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        let version = service.current_task()?.unwrap().version;
        let later = service.begin_run(
            VersionedTask {
                id: later_task,
                version,
            },
            "later-success",
            "fixture-base",
            false,
        )?;
        service.finish_run(&later, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let evidence = state.join("evidence");
        let first_directory = evidence.join(format!("attempt-{}", first.attempt));
        let later_directory = evidence.join(format!("attempt-{}", later.attempt));
        std::fs::create_dir_all(&first_directory)?;
        std::fs::create_dir_all(&later_directory)?;
        let first_revision: i64 = service.db.connection().query_row(
            "SELECT revision FROM attempts WHERE id=?1",
            [first.attempt],
            |row| row.get(0),
        )?;
        service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,1,'check','succeeded','true','.',1000,1,2)",params![first.attempt,service.project_id,first_task,first_revision])?;
        let first_step = service.db.connection().last_insert_rowid();
        std::fs::create_dir(first_directory.join(format!("step-{first_step}")))?;
        let later_revision: i64 = service.db.connection().query_row(
            "SELECT revision FROM attempts WHERE id=?1",
            [later.attempt],
            |row| row.get(0),
        )?;
        for ordinal in 1..=80 {
            service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)",params![later.attempt,service.project_id,later_task,later_revision,ordinal])?;
            let step = service.db.connection().last_insert_rowid();
            std::fs::create_dir(later_directory.join(format!("step-{step}")))?;
        }
        Ok(SweepFixture {
            service,
            first_attempt: first.attempt,
            first_step,
            later_attempt: later.attempt,
            first_directory,
            later_directory,
            state,
            project,
            _sandbox: sandbox,
        })
    }

    fn evidence_counts(root: &Path) -> Result<(usize, usize)> {
        if !root.exists() {
            return Ok((0, 0));
        }
        let mut files = 0;
        let mut directories = 0;
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories += 1;
                let children = evidence_counts(&entry.path())?;
                files += children.0;
                directories += children.1;
            } else {
                files += 1;
            }
        }
        Ok((files, directories))
    }

    fn drain_sweep_by_receipt(service: &mut Service, evidence: &Path) -> Result<Vec<bool>> {
        let mut limits = Vec::new();
        for _ in 0..6 {
            let preview = service.storage_preview()?;
            let receipt = service.cleanup_storage(&preview)?;
            let (files, directories) = evidence_counts(evidence)?;
            println!(
                "sweep receipt: files={files} directories={directories} batch_limit={}",
                receipt.batch_limit_reached
            );
            limits.push(receipt.batch_limit_reached);
            if !receipt.batch_limit_reached {
                return Ok(limits);
            }
        }
        Err(Error::Incompatible("Bounded sweep did not finish".into()))
    }

    fn reserve_first(service: &Service, attempt: i64, step: i64) -> Result<()> {
        let relative = format!("evidence/attempt-{attempt}/step-{step}/file-1-0");
        service.db.connection().execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,'interrupted','text/plain',?2,0,?3,?4,1)",params![attempt,relative,"0".repeat(64),"0".repeat(40)])?;
        Ok(())
    }

    #[test]
    fn output_batches_keep_original_size_and_complete_marker() -> Result<()> {
        let (sandbox, mut service, task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "output-batches",
            "fixture-base",
            false,
        )?;
        for _ in 0..80 {
            service.output(&run, "stdout", "row")?;
        }
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let preview = service.storage_preview()?;
        let first = service.cleanup_storage(&preview)?;
        assert!(first.batch_limit_reached);
        let remaining: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        assert_eq!(remaining, 16);
        let usage: (i64, i64) = service.db.connection().query_row(
            "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
            [run.attempt],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(usage, (48, 16));
        let marker: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM output_pruning WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        assert_eq!(marker, 0);
        let partial = &service.task(task)?.unwrap().attempts[0];
        assert!(!partial.output_pruned);
        assert_eq!(partial.output_pruning_bytes, 192);
        assert_eq!(partial.output_pruning_records, 64);
        assert!(
            service
                .browse_output_at(
                    run.attempt,
                    0,
                    0,
                    None,
                    service.output_watermark(run.attempt)?
                )?
                .contains("PARTIALLY PRUNED: 192")
        );
        drop(service);
        let mut service = Service::open(
            crate::storage::Database::open_in(&sandbox.path().join("state"))?,
            &sandbox.path().join("project"),
            None,
        )?;
        assert_eq!(
            service.task(task)?.unwrap().attempts[0].output_pruning_bytes,
            192
        );
        assert_eq!(
            service.task(task)?.unwrap().attempts[0].output_pruning_records,
            64
        );
        let next = service.storage_preview()?;
        let second = service.cleanup_storage(&next)?;
        assert_eq!(second.bytes, 48);
        let size: i64 = service.db.connection().query_row(
            "SELECT size_bytes FROM output_pruning WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        assert_eq!(size, 240);
        let progress: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM output_pruning_progress WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        assert_eq!(progress, 0);
        let admission: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM evidence_cleanup_admissions WHERE attempt_id=?1",
            [run.attempt],
            |row| row.get(0),
        )?;
        assert_eq!(admission, 0);
        let complete = &service.task(task)?.unwrap().attempts[0];
        assert!(complete.output_pruned);
        assert_eq!(complete.output_pruning_bytes, 0);
        assert_eq!(complete.output_pruning_records, 0);
        Ok(())
    }

    #[test]
    fn empty_output_rows_distinguish_deletion_from_pending_intent() -> Result<()> {
        let (_sandbox, mut service, task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "empty-output",
            "fixture-base",
            false,
        )?;
        for _ in 0..65 {
            service.output(&run, "stdout", "")?;
        }
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let preview = service.storage_preview()?;
        service.cleanup_storage(&preview)?;
        let partial = &service.task(task)?.unwrap().attempts[0];
        assert_eq!(partial.output_pruning_bytes, 0);
        assert_eq!(partial.output_pruning_records, 64);
        let usage: (i64, i64) = service.db.connection().query_row(
            "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
            [run.attempt],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(usage, (0, 1));
        assert!(
            service
                .browse_output_at(
                    run.attempt,
                    0,
                    0,
                    None,
                    service.output_watermark(run.attempt)?
                )?
                .contains("PARTIALLY PRUNED: 0")
        );
        Ok(())
    }

    #[test]
    fn empty_step_cleanup_resumes_inside_large_attempt() -> Result<()> {
        let (sandbox, mut service, task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "many-steps",
            "fixture-base",
            false,
        )?;
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let directory = sandbox
            .path()
            .join(format!("state/evidence/attempt-{}", run.attempt));
        std::fs::create_dir_all(&directory)?;
        let revision: i64 = service.db.connection().query_row(
            "SELECT revision FROM attempts WHERE id=?1",
            [run.attempt],
            |row| row.get(0),
        )?;
        for ordinal in 1..=80 {
            service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)", params![run.attempt,service.project_id,task,revision,ordinal])?;
            let step = service.db.connection().last_insert_rowid();
            std::fs::create_dir(directory.join(format!("step-{step}")))?;
        }
        let preview = service.storage_preview()?;
        let first = service.cleanup_storage(&preview)?;
        assert!(first.batch_limit_reached);
        assert_eq!(std::fs::read_dir(&directory)?.count(), 16);
        let active: i64 = service.db.connection().query_row(
            "SELECT active_attempt_id FROM evidence_sweep WHERE project_id=?1",
            [service.project_id],
            |row| row.get(0),
        )?;
        assert_eq!(active, run.attempt);
        let preview = service.storage_preview()?;
        let second = service.cleanup_storage(&preview)?;
        assert_eq!(second.attempts, 1);
        assert!(!directory.exists());
        Ok(())
    }

    #[test]
    fn pending_artifact_keeps_output_eligible_across_batches() -> Result<()> {
        let (sandbox, mut service, task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "pending-output",
            "fixture-base",
            false,
        )?;
        for _ in 0..80 {
            service.output(&run, "stdout", "row")?;
        }
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 365000)?;
        let directory = sandbox
            .path()
            .join(format!("state/evidence/attempt-{}", run.attempt));
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join("step-987.log"), "pending")?;
        service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms,pruning_requested_at_ms) VALUES (?1,'pending','text/plain',?2,7,1,2)", params![run.attempt,format!("evidence/attempt-{}/step-987.log",run.attempt)])?;
        let preview = service.storage_preview()?;
        assert_eq!(preview.candidates.len(), 1);
        let first = service.cleanup_storage(&preview)?;
        assert!(first.batch_limit_reached);
        let preview = service.storage_preview()?;
        assert_eq!(preview.candidates.len(), 1);
        let second = service.cleanup_storage(&preview)?;
        assert_eq!(second.bytes, 51);
        assert!(
            !directory.exists(),
            "admitted cleanup left an empty directory after the policy changed"
        );
        let remaining: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [run.attempt],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 0);
        Ok(())
    }

    #[test]
    fn sweep_receipt_requests_wrap_for_earlier_newly_closed_attempt() -> Result<()> {
        let (sandbox, mut service, first_task) = closed_service()?;
        let version = service.current_task()?.unwrap().version;
        let first = service.begin_run(
            VersionedTask {
                id: first_task,
                version,
            },
            "earlier-failure",
            "fixture-base",
            false,
        )?;
        service.finish_run(&first, "failed", Some("check"), "Failed", None)?;
        let version = service.current_task()?.unwrap().version;
        service.mark_done(
            VersionedTask {
                id: first_task,
                version,
            },
            "Resolved",
        )?;
        let models = std::array::from_fn(|_| {
            Some(ModelConfig {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: None,
            })
        });
        let later_task = service.create(
            &[Definition {
                title: "Later".into(),
                goal: "Sweep".into(),
                kind: Kind::Ai,
                criteria: vec!["Done".into()],
                models,
                attempt_limit: Some(1),
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        let version = service.current_task()?.unwrap().version;
        let later = service.begin_run(
            VersionedTask {
                id: later_task,
                version,
            },
            "later-success",
            "fixture-base",
            false,
        )?;
        service.finish_run(&later, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let evidence = sandbox.path().join("state/evidence");
        let first_directory = evidence.join(format!("attempt-{}", first.attempt));
        let later_directory = evidence.join(format!("attempt-{}", later.attempt));
        std::fs::create_dir_all(&first_directory)?;
        std::fs::create_dir_all(&later_directory)?;
        let revision: i64 = service.db.connection().query_row(
            "SELECT revision FROM attempts WHERE id=?1",
            [later.attempt],
            |row| row.get(0),
        )?;
        for ordinal in 1..=80 {
            service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)",params![later.attempt,service.project_id,later_task,revision,ordinal])?;
            let step = service.db.connection().last_insert_rowid();
            std::fs::create_dir(later_directory.join(format!("step-{step}")))?;
        }
        service.db.connection().execute(
            "UPDATE tasks SET lifecycle='blocked',closed_at_ms=NULL WHERE id=?1",
            [first_task],
        )?;
        let preview = service.storage_preview()?;
        let first_receipt = service.cleanup_storage(&preview)?;
        assert!(first_receipt.batch_limit_reached);
        assert_eq!(std::fs::read_dir(&later_directory)?.count(), 16);
        service.db.connection().execute(
            "UPDATE tasks SET lifecycle='failed_skipped',closed_at_ms=?2 WHERE id=?1",
            params![first_task, now_ms()],
        )?;
        let mut receipts = Vec::new();
        for _ in 0..4 {
            let preview = service.storage_preview()?;
            let receipt = service.cleanup_storage(&preview)?;
            let done = !receipt.batch_limit_reached;
            receipts.push(receipt.batch_limit_reached);
            if done {
                break;
            }
        }
        assert_eq!(
            receipts,
            vec![true, false],
            "cleanup must request the wrapped pass and then finish"
        );
        assert!(!first_directory.exists());
        assert!(!later_directory.exists());
        Ok(())
    }

    #[test]
    fn released_reservation_restarts_bounded_sweep_after_reopening() -> Result<()> {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        reserve_first(&service, fixture.first_attempt, fixture.first_step)?;
        let preview = service.storage_preview()?;
        let first = service.cleanup_storage(&preview)?;
        assert!(first.batch_limit_reached);
        assert_eq!(evidence_counts(&fixture.state.join("evidence"))?, (0, 19));
        assert!(fixture.first_directory.exists());
        drop(service);
        let mut service = Service::open(
            crate::storage::Database::open_in(&fixture.state)?,
            &fixture.project,
            None,
        )?;
        let reservations: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM artifact_reservations WHERE attempt_id=?1",
            [fixture.first_attempt],
            |row| row.get(0),
        )?;
        assert_eq!(
            reservations, 0,
            "reopening must reconcile the interrupted publication"
        );
        let dirty: bool = service.db.connection().query_row(
            "SELECT dirty FROM evidence_sweep WHERE project_id=?1",
            [service.project_id],
            |row| row.get(0),
        )?;
        assert!(dirty);
        let limits = drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
        assert!(limits.len() >= 2, "the later attempt exceeds one batch");
        assert!(
            !fixture.first_directory.exists(),
            "an earlier released reservation was stranded"
        );
        assert!(!fixture.later_directory.exists());
        assert_eq!(evidence_counts(&fixture.state.join("evidence"))?, (0, 0));
        Ok(())
    }

    #[test]
    fn reserved_attempt_does_not_keep_receipts_open() -> Result<()> {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        reserve_first(&service, fixture.first_attempt, fixture.first_step)?;
        let limits = drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
        assert!(limits.len() >= 2);
        assert!(fixture.first_directory.exists());
        assert!(!fixture.later_directory.exists());
        let reservations: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM artifact_reservations WHERE attempt_id=?1",
            [fixture.first_attempt],
            |row| row.get(0),
        )?;
        assert_eq!(reservations, 1);
        Ok(())
    }

    #[test]
    fn lowering_retention_restarts_behind_cursor() -> Result<()> {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 365000)?;
        service.db.connection().execute(
            "INSERT INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,1)",
            [fixture.later_attempt],
        )?;
        let preview = service.storage_preview()?;
        let first = service.cleanup_storage(&preview)?;
        assert!(first.batch_limit_reached);
        assert!(fixture.first_directory.exists());
        assert_eq!(std::fs::read_dir(&fixture.later_directory)?.count(), 16);
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
        assert!(!fixture.first_directory.exists());
        assert!(!fixture.later_directory.exists());
        Ok(())
    }

    #[test]
    fn completed_sweep_reopens_after_age_deadline_without_a_mutation() -> Result<()> {
        let (sandbox, mut service, task) = closed_service()?;
        let definition = service.task(task)?.unwrap().definition().definition.clone();
        let version = service.current_task()?.unwrap().version;
        let first = service.begin_run(
            VersionedTask { id: task, version },
            "aging-first",
            "fixture-base",
            false,
        )?;
        service.finish_run(&first, "succeeded", None, "Done", None)?;
        let next = service.create(&[definition], None, true)?[0];
        let version = service.current_task()?.unwrap().version;
        let second = service.begin_run(
            VersionedTask { id: next, version },
            "aging-second",
            "fixture-base",
            false,
        )?;
        service.finish_run(&second, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 1)?;
        let deadline = now_ms() + 3000;
        service.db.connection().execute(
            "UPDATE attempts SET started_at_ms=?2,finished_at_ms=?3 WHERE id=?1",
            params![first.attempt, deadline - 86_400_001, deadline - 86_400_000],
        )?;
        let state = sandbox.path().join("state");
        let first_dir = state.join(format!("evidence/attempt-{}", first.attempt));
        let second_dir = state.join(format!("evidence/attempt-{}", second.attempt));
        std::fs::create_dir_all(&first_dir)?;
        std::fs::create_dir_all(&second_dir)?;
        service.automatic_housekeeping()?;
        let (complete, next_eligible): (bool, i64) = service.db.connection().query_row(
            "SELECT complete,next_eligible_at_ms FROM evidence_sweep WHERE project_id=?1",
            [service.project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert!(complete);
        assert_eq!(next_eligible, deadline);
        assert!(first_dir.exists() && second_dir.exists());
        let changes_before = service.db.connection().total_changes();
        let mut budget = CLEANUP_BATCH;
        sweep_empty_evidence(
            service.db.connection(),
            service.db.state_dir(),
            service.project_id,
            &mut budget,
            false,
            false,
        )?;
        assert_eq!(
            service.db.connection().total_changes(),
            changes_before,
            "a pre-deadline sweep restarted"
        );
        assert!(first_dir.exists() && second_dir.exists());
        drop(service);
        std::thread::sleep(std::time::Duration::from_millis(
            (deadline - now_ms()).max(0) as u64 + 30,
        ));
        let mut reopened = Service::open(
            crate::storage::Database::open_in(&state)?,
            &sandbox.path().join("project"),
            None,
        )?;
        reopened.automatic_housekeeping()?;
        assert!(
            !first_dir.exists(),
            "the newly eligible empty directory was stranded"
        );
        assert!(second_dir.exists(), "a younger directory was removed");
        Ok(())
    }

    #[test]
    fn preserved_step_does_not_keep_receipts_open() -> Result<()> {
        use std::os::unix::fs::symlink;
        let fixture = sweep_fixture()?;
        let target = fixture.state.join("unrelated");
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("keep"), "preserved")?;
        let step = fixture
            .first_directory
            .join(format!("step-{}", fixture.first_step));
        std::fs::remove_dir(&step)?;
        symlink(&target, &step)?;
        let mut service = fixture.service;
        let limits = drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
        assert!(limits.len() >= 2);
        assert!(step.symlink_metadata()?.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(target.join("keep"))?, "preserved");
        assert!(!fixture.later_directory.exists());
        Ok(())
    }

    #[test]
    fn directory_phase_rechecks_protection_acquired_after_file_phase() -> Result<()> {
        let sandbox =
            crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let project = sandbox.path().join("project");
        std::fs::create_dir(&project)?;
        let state = sandbox.path().join("state");
        let mut service =
            Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
        let models = std::array::from_fn(|_| {
            Some(ModelConfig {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: None,
            })
        });
        let task = service.create(
            &[Definition {
                title: "Closed".into(),
                goal: "Test cleanup".into(),
                kind: Kind::Ai,
                criteria: vec!["Done".into()],
                models,
                attempt_limit: Some(1),
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "retention-guard",
            "fixture-base",
            false,
        )?;
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        let policy = service.retention_policy()?;
        service.set_retention(policy, 0)?;
        let directory = state.join(format!("evidence/attempt-{}", run.attempt));
        std::fs::create_dir_all(&directory)?;
        // This call represents the directory phase after the file transaction
        // committed. Another connection acquired a project-level provider call.
        let other = crate::storage::Database::open_in(&state)?;
        other.connection().execute("INSERT INTO model_calls(project_id,role,provider,model,request_key,state,started_at_ms) VALUES (?1,'planning','claude','sonnet','guard-call','running',1)", [service.project_id])?;
        let tx =
            Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
        let mut budget = CLEANUP_BATCH;
        assert!(
            sweep_empty_evidence(
                &tx,
                service.db.state_dir(),
                service.project_id,
                &mut budget,
                false,
                false,
            )
            .is_err()
        );
        tx.rollback()?;
        assert!(directory.exists());
        other.connection().execute("UPDATE model_calls SET state='succeeded',finished_at_ms=2 WHERE request_key='guard-call'", [])?;
        let tx =
            Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
        let mut budget = CLEANUP_BATCH;
        sweep_empty_evidence(
            &tx,
            service.db.state_dir(),
            service.project_id,
            &mut budget,
            false,
            false,
        )?;
        tx.commit()?;
        assert!(!directory.exists());
        Ok(())
    }
}

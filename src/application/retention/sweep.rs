//! Removal of empty evidence directories of eligible closed attempts. A durable cursor
//! limits both the number of attempts and the number of rows within a large attempt; only
//! recorded steps and artifact parents are owned. Directories are removed with no
//! transaction open; one short transaction then records the cursor.
use super::candidates::protected;
use super::database::unsynced;
use super::policy::policy;
use super::*;
use crate::application::evidence_path::EvidencePath;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::Mode,
    unistd::{UnlinkatFlags, unlinkat},
};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;

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

/// Remove `name` if it is an empty directory; the caller syncs `parent` once.
fn remove_if_empty(parent: &File, name: &str) -> Result<bool> {
    match unlinkat(parent, name, UnlinkatFlags::RemoveDir) {
        Ok(()) => Ok(true),
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

/// The directories one call removed, and what it must record.
pub(super) struct Sweep {
    project: i64,
    visited: Vec<i64>,
    admitted: Vec<i64>,
    released: Vec<i64>,
    cursor: Option<SweepCursor>,
}

impl Sweep {
    /// Record the sweep in the caller's write transaction; returns the attempts whose
    /// directory is gone.
    pub(super) fn record(self, tx: &Connection) -> Result<Vec<i64>> {
        let now = now_ms();
        for attempt in &self.admitted {
            tx.execute("INSERT OR IGNORE INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,?2)", params![attempt,now])?;
        }
        for attempt in &self.released {
            tx.execute(
                "DELETE FROM evidence_cleanup_admissions WHERE attempt_id=?1",
                [attempt],
            )?;
        }
        if let Some(cursor) = self.cursor {
            tx.execute("INSERT INTO evidence_sweep(project_id,last_attempt_id,active_attempt_id,last_step_id,last_artifact_id,dirty,complete,pressure,next_eligible_at_ms) VALUES (?1,?2,?3,?4,?5,0,?6,?7,?8) ON CONFLICT(project_id) DO UPDATE SET last_attempt_id=excluded.last_attempt_id,active_attempt_id=excluded.active_attempt_id,last_step_id=excluded.last_step_id,last_artifact_id=excluded.last_artifact_id,dirty=0,complete=excluded.complete,pressure=excluded.pressure,next_eligible_at_ms=excluded.next_eligible_at_ms", params![self.project,cursor.last_attempt,cursor.active_attempt,cursor.last_step,cursor.last_artifact,cursor.complete,cursor.pressure,cursor.next_eligible_at_ms])?;
        }
        Ok(self.visited)
    }
}

/// Remove empty directories within `budget` and sync each changed parent once.
pub(super) fn sweep_empty_evidence(
    db: &Connection,
    root: &File,
    project: i64,
    budget: &mut i64,
    pressure: bool,
    rescan_complete: bool,
) -> Result<Sweep> {
    require(
        !protected(db, project)?,
        "Storage or active work changed during cleanup; preview again",
    )?;
    let retention = policy(db, project)?;
    let now = now_ms();
    let cutoff = retention.cutoff(now);
    let mut cursor = db.query_row("SELECT last_attempt_id,active_attempt_id,last_step_id,last_artifact_id,dirty,complete,pressure,next_eligible_at_ms FROM evidence_sweep WHERE project_id=?1", [project], |r| Ok(SweepCursor {last_attempt:r.get(0)?,active_attempt:r.get(1)?,last_step:r.get(2)?,last_artifact:r.get(3)?,dirty:r.get(4)?,complete:r.get(5)?,pressure:r.get(6)?,next_eligible_at_ms:r.get(7)?})).optional()?.unwrap_or_default();
    let mut sweep = Sweep {
        project,
        visited: Vec::new(),
        admitted: Vec::new(),
        released: Vec::new(),
        cursor: None,
    };
    if cursor.dirty || cursor.pressure != pressure {
        cursor = SweepCursor::default();
    }
    cursor.pressure = pressure;
    let age_due = cursor.next_eligible_at_ms != 0 && cursor.next_eligible_at_ms <= now;
    if cursor.complete && !rescan_complete && !age_due {
        return Ok(sweep);
    }
    if cursor.complete {
        cursor = SweepCursor {
            pressure,
            ..SweepCursor::default()
        };
    }
    const ELIGIBLE: &str = "SELECT a.id FROM unreserved a WHERE (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM evidence_cleanup_admissions d WHERE d.attempt_id=a.id))";
    // Evidence directories with removed attempts, by inode; each is synced once.
    let mut changed_evidence = BTreeMap::new();
    while *budget > 0 {
        let active: Option<i64> = db
            .query_row(
                &format!("{CLOSED} {ELIGIBLE} AND a.id=?3"),
                params![project, cutoff, cursor.active_attempt],
                |r| r.get(0),
            )
            .optional()?;
        let attempt = if let Some(id) = active {
            id
        } else {
            cursor.active_attempt = 0;
            cursor.last_step = 0;
            cursor.last_artifact = 0;
            let next: Option<i64> = db
                .query_row(
                    &format!("{CLOSED} {ELIGIBLE} AND a.id>?3 ORDER BY a.id LIMIT 1"),
                    params![project, cutoff, cursor.last_attempt],
                    |r| r.get(0),
                )
                .optional()?;
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
                    sweep.released.push(attempt);
                }
                cursor.last_attempt = attempt;
                cursor.active_attempt = 0;
                *budget -= 1;
                continue;
            }
        };
        sweep.admitted.push(attempt);
        let mut changed = false;
        let steps = db
            .prepare(
                "SELECT id FROM attempt_steps WHERE attempt_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            )?
            .query_map(params![attempt, cursor.last_step, *budget], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for step in steps {
            changed |= remove_if_empty(&directory, &format!("step-{step}"))?;
            cursor.last_step = step;
            *budget -= 1;
        }
        if *budget > 0 {
            let files = db.prepare("SELECT id,relative_path FROM artifacts WHERE attempt_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?.query_map(params![attempt,cursor.last_artifact,*budget], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, path) in files {
                if let Some(step_directory) = EvidencePath::parse(&path)
                    .filter(|path| path.attempt == attempt && path.step().is_some())
                    .and_then(|path| path.directory)
                {
                    changed |= remove_if_empty(&directory, step_directory)?;
                }
                cursor.last_artifact = id;
                *budget -= 1;
            }
        }
        if changed {
            directory.sync_all()?;
        }
        if *budget == 0 {
            break;
        }
        if remove_if_empty(&evidence, &format!("attempt-{attempt}"))? {
            sweep.visited.push(attempt);
            sweep.released.push(attempt);
            let identity = evidence.metadata()?;
            changed_evidence
                .entry((identity.dev(), identity.ino()))
                .or_insert(evidence);
        }
        cursor.last_attempt = attempt;
        cursor.active_attempt = 0;
        cursor.last_step = 0;
        cursor.last_artifact = 0;
        *budget -= 1;
    }
    for evidence in changed_evidence.values() {
        evidence.sync_all()?;
    }
    if cursor.complete {
        cursor.next_eligible_at_ms = db.query_row(&format!("{CLOSED} SELECT min(a.finished_at_ms+?2) FROM unreserved a WHERE a.finished_at_ms>?3"), params![project,retention.retention_ms(),cutoff], |r| r.get::<_,Option<i64>>(0))?.unwrap_or(0);
    }
    sweep.cursor = Some(cursor);
    Ok(sweep)
}

/// One sweep step of a cleanup batch, recorded in its own short write transaction.
pub(super) fn sweep_batch(
    db: &Connection,
    root: &File,
    project: i64,
    budget: &mut i64,
    pressure: bool,
    interactive: bool,
) -> Result<Vec<i64>> {
    let needed: bool = interactive || db.query_row("SELECT NOT EXISTS(SELECT 1 FROM evidence_sweep WHERE project_id=?1 AND complete=1 AND dirty=0 AND pressure=?2 AND (next_eligible_at_ms=0 OR next_eligible_at_ms>?3))", params![project,pressure,now_ms()], |r| r.get(0))?;
    let sweep = sweep_empty_evidence(db, root, project, budget, pressure, interactive)?;
    if !needed && sweep.cursor.is_none() {
        return Ok(Vec::new());
    }
    // A lost cursor only repeats the sweep over directories already gone.
    unsynced(db, || {
        let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        let visited = sweep.record(&tx)?;
        if needed {
            bump_admission_generation(&tx)?;
        }
        tx.commit()?;
        Ok(visited)
    })
}

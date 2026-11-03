//! Deleting one batch of eligible evidence in three phases: a short write transaction
//! records the delete intent, files are unlinked with no transaction open, and a second
//! short transaction marks them pruned, deletes output rows and returns one step of free
//! pages. An interrupted batch leaves its intent, so a missing file is an explicit,
//! retryable cleanup operation. A batch without files needs only the first transaction.
use super::candidates::{OUTPUT_BYTES, PreviewMode, preview};
use super::database::{sync_wal, unsynced, vacuum_free_pages};
use super::measure::Allocation;
use super::*;
use crate::application::evidence_path::EvidencePath;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::Mode,
    unistd::{UnlinkatFlags, unlinkat},
};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

const DIRECTORY: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);

pub(super) struct Selected {
    id: i64,
    attempt: i64,
    path: String,
    size: i64,
}

/// What the intent transaction committed to delete. With no file to unlink it has
/// already deleted the output rows itself and `recorded` holds the result.
pub(super) struct Intent {
    preview: Preview,
    cutoff: i64,
    pub selected: Vec<Selected>,
    pub recorded: Option<Recorded>,
}

/// The result of the unlink phase. A path that is not one ktask generates, or whose
/// parent was substituted, is skipped and reported; any other error stops the phase.
#[derive(Default)]
pub(super) struct Unlinked {
    removed: Vec<usize>,
    pub skipped: Vec<String>,
    pub failure: Option<Error>,
}

/// What the batch recorded and how much of its budget remains.
pub(super) struct Recorded {
    pub attempts: BTreeSet<i64>,
    pub bytes: i64,
    pub budget: i64,
    /// The rechecked launch pressure, which selects the sweep's eligibility.
    pub pressure: bool,
}

pub(super) fn record_intent(
    db: &Connection,
    root: &File,
    project: i64,
    expected: &Preview,
    observed: &Allocation,
    mode: PreviewMode,
) -> Result<Intent> {
    let intent = unsynced(db, || {
        let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        let intent = intend(&tx, root, project, expected, observed, mode)?;
        tx.commit()?;
        Ok(intent)
    })?;
    // The intent must survive a crash before any file it names is unlinked.
    if !intent.selected.is_empty() {
        sync_wal(root)?;
    }
    Ok(intent)
}

fn intend(
    tx: &Connection,
    root: &File,
    project: i64,
    expected: &Preview,
    observed: &Allocation,
    mode: PreviewMode,
) -> Result<Intent> {
    let now = now_ms();
    let current = preview(tx, root, project, now, observed, mode)?;
    require(
        !current.deferred_for_live_work
            && current.policy == expected.policy
            && current.candidates == expected.candidates,
        "Storage changed since preview; preview again",
    )?;
    if !current.candidates.is_empty() {
        bump_admission_generation(tx)?;
    }
    let cutoff = current.policy.cutoff(now);
    let selected = tx.prepare(&format!("{CLOSED} SELECT f.id,f.attempt_id,f.relative_path,f.size_bytes FROM unreserved a JOIN artifacts f ON f.attempt_id=a.id WHERE f.pruned_at_ms IS NULL AND (a.finished_at_ms<=?2 OR f.pruning_requested_at_ms IS NOT NULL) ORDER BY f.id LIMIT ?3"))?.query_map(params![project,cutoff,CLEANUP_BATCH], |r| Ok(Selected { id: r.get(0)?, attempt: r.get(1)?, path: r.get(2)?, size: r.get(3)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for file in &selected {
        tx.execute("UPDATE artifacts SET pruning_requested_at_ms=max(created_at_ms,?2) WHERE id=?1 AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NULL", params![file.id,now])?;
    }
    for attempt in selected
        .iter()
        .map(|file| file.attempt)
        .collect::<BTreeSet<_>>()
    {
        tx.execute("INSERT OR IGNORE INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,?2)", params![attempt,now])?;
        tx.execute("INSERT OR IGNORE INTO output_pruning_progress(attempt_id,size_bytes) SELECT ?1,0 WHERE EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=?1)", [attempt])?;
    }
    // Nothing to unlink: one transaction both decides and deletes.
    let recorded = if selected.is_empty() {
        let mut recorded = Recorded {
            attempts: BTreeSet::new(),
            bytes: 0,
            budget: CLEANUP_BATCH,
            pressure: current.launch_capacity_reached,
        };
        prune_output(tx, project, cutoff, &mut recorded)?;
        if !recorded.attempts.is_empty() {
            bump_admission_generation(tx)?;
        }
        vacuum_free_pages(tx)?;
        Some(recorded)
    } else {
        None
    };
    Ok(Intent {
        preview: current,
        cutoff,
        selected,
        recorded,
    })
}

/// A file's parent, opened from the state root through descriptors with O_NOFOLLOW, so a
/// swapped symlink cannot redirect deletion into a project.
enum Parent {
    Missing,
    Substituted,
    Open(File),
}

fn open_parent(root: &File, path: &EvidencePath<'_>) -> Result<Parent> {
    let mut parent = root.try_clone()?;
    for name in path.parents() {
        parent = match openat(&parent, name, DIRECTORY, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(Errno::ENOENT) => return Ok(Parent::Missing),
            Err(Errno::ENOTDIR | Errno::ELOOP) => return Ok(Parent::Substituted),
            Err(error) => return Err(io(error)),
        };
    }
    Ok(Parent::Open(parent))
}

/// Allocated bytes of one generated evidence file, or zero if it cannot be resolved.
pub(super) fn allocated(root: &File, path: &EvidencePath<'_>) -> i64 {
    let Ok(Parent::Open(parent)) = open_parent(root, path) else {
        return 0;
    };
    std::fs::symlink_metadata(format!(
        "/proc/self/fd/{}/{}",
        parent.as_raw_fd(),
        path.name
    ))
    .map(|meta| (meta.blocks() as i64).saturating_mul(512))
    .unwrap_or(0)
}

/// Unlink the intended files with no transaction open, then sync each parent once.
pub(super) fn unlink(root: &File, selected: &[Selected]) -> Unlinked {
    let mut unlinked = Unlinked {
        removed: Vec::new(),
        skipped: Vec::new(),
        failure: None,
    };
    // Parents by path, each synced once after its last unlink; a file counts as removed
    // only once its parent is durable.
    let mut parents: BTreeMap<String, (File, Vec<usize>)> = BTreeMap::new();
    for (index, file) in selected.iter().enumerate() {
        let Some(path) = EvidencePath::parse(&file.path)
            .filter(|path| path.attempt == file.attempt && path.generated())
        else {
            unlinked.skipped.push(format!(
                "Unsafe artifact path {:?} of attempt {} skipped",
                file.path, file.attempt
            ));
            continue;
        };
        let key = path.parents().collect::<Vec<_>>().join("/");
        if !parents.contains_key(&key) {
            match open_parent(root, &path) {
                Ok(Parent::Open(parent)) => {
                    parents.insert(key.clone(), (parent, Vec::new()));
                }
                Ok(Parent::Missing) => {
                    unlinked.removed.push(index);
                    continue;
                }
                Ok(Parent::Substituted) => {
                    unlinked.skipped.push(format!(
                        "Substituted evidence directory for {:?} skipped",
                        file.path
                    ));
                    continue;
                }
                Err(error) => {
                    unlinked.failure = Some(error);
                    break;
                }
            }
        }
        let (parent, removed) = parents.get_mut(&key).expect("opened parent");
        match unlinkat(&*parent, path.name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => removed.push(index),
            Err(error) => {
                unlinked.failure = Some(io(error));
                break;
            }
        }
    }
    for (parent, removed) in parents.into_values() {
        if removed.is_empty() {
            continue;
        }
        match parent.sync_all() {
            Ok(()) => unlinked.removed.extend(removed),
            Err(error) => {
                unlinked.failure.get_or_insert(error.into());
            }
        }
    }
    unlinked
}

/// Mark the unlinked files pruned and, if nothing changed since the intent, delete output
/// rows with the remaining budget. Files already gone are recorded even when the batch
/// must stop because live work appeared. Losing this commit only repeats it: the intent
/// keeps the files eligible and an already missing file counts as removed.
pub(super) fn record_deletion(
    db: &Connection,
    root: &File,
    project: i64,
    intent: &Intent,
    unlinked: &Unlinked,
    observed: &Allocation,
    mode: PreviewMode,
) -> Result<Recorded> {
    let (recorded, unchanged) = unsynced(db, || {
        let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        let rechecked = preview(&tx, root, project, now_ms(), observed, mode)?;
        let by_id: BTreeMap<_, _> = rechecked
            .candidates
            .iter()
            .map(|candidate| (candidate.attempt_id, candidate))
            .collect();
        let unchanged = !rechecked.deferred_for_live_work
            && rechecked.policy == intent.preview.policy
            && intent.preview.candidates.iter().all(|before| {
                by_id.get(&before.attempt_id).is_some_and(|after| {
                    before.task_id == after.task_id
                        && before.output_bytes == after.output_bytes
                        && before.output_records == after.output_records
                        && before.artifact_bytes == after.artifact_bytes
                        && before.artifacts == after.artifacts
                })
            });
        let mut recorded = Recorded {
            attempts: BTreeSet::new(),
            bytes: 0,
            budget: CLEANUP_BATCH,
            pressure: rechecked.launch_capacity_reached,
        };
        let now = now_ms();
        for file in unlinked
            .removed
            .iter()
            .map(|&index| &intent.selected[index])
        {
            tx.execute("UPDATE artifacts SET pruned_at_ms=max(created_at_ms,?2),pruning_requested_at_ms=NULL WHERE id=?1 AND pruned_at_ms IS NULL", params![file.id, now])?;
            recorded.attempts.insert(file.attempt);
            recorded.bytes = recorded.bytes.saturating_add(file.size);
            recorded.budget -= 1;
        }
        if unchanged && unlinked.failure.is_none() && recorded.budget > 0 {
            prune_output(&tx, project, intent.cutoff, &mut recorded)?;
        }
        if !recorded.attempts.is_empty() {
            bump_admission_generation(&tx)?;
        }
        vacuum_free_pages(&tx)?;
        tx.commit()?;
        Ok((recorded, unchanged))
    })?;
    require(
        unchanged,
        "Storage or active work changed during cleanup; preview again",
    )?;
    Ok(recorded)
}

/// Output rows are deleted inside the transaction, each attempt's oldest rows by one
/// range; the trigger-maintained counters give their size without reading content. An
/// attempt whose last row goes records its original size in `output_pruning`.
fn prune_output(tx: &Connection, project: i64, cutoff: i64, recorded: &mut Recorded) -> Result<()> {
    let rows = tx.prepare(&format!("{CLOSED} SELECT o.attempt_id,o.sequence FROM unreserved a JOIN attempt_output o ON o.attempt_id=a.id WHERE (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM output_pruning_progress WHERE attempt_id=a.id)) ORDER BY o.attempt_id,o.sequence LIMIT ?3"))?.query_map(params![project,cutoff,recorded.budget], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    // Rows arrive in (attempt, sequence) order, so each attempt's selection is every
    // retained row up to its last selected sequence.
    let mut last = BTreeMap::new();
    for (attempt, sequence) in rows {
        last.insert(attempt, sequence);
    }
    let size = format!("SELECT {OUTPUT_BYTES} FROM attempts a WHERE a.id=?1");
    let now = now_ms();
    for (attempt, sequence) in last {
        tx.execute("INSERT OR IGNORE INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,?2)", params![attempt,now])?;
        let before: i64 = tx.query_row(&size, [attempt], |r| r.get(0))?;
        let removed = tx.execute(
            "DELETE FROM attempt_output WHERE attempt_id=?1 AND sequence<=?2",
            params![attempt, sequence],
        )? as i64;
        let bytes = before.saturating_sub(tx.query_row(&size, [attempt], |r| r.get(0))?);
        tx.execute("INSERT INTO output_pruning_progress(attempt_id,size_bytes,removed_records) VALUES (?1,?2,?3) ON CONFLICT(attempt_id) DO UPDATE SET size_bytes=size_bytes+excluded.size_bytes,removed_records=removed_records+excluded.removed_records", params![attempt,bytes,removed])?;
        let remains: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=?1)",
            [attempt],
            |r| r.get(0),
        )?;
        if !remains {
            tx.execute("INSERT OR IGNORE INTO output_pruning(attempt_id,pruned_at_ms,size_bytes) SELECT attempt_id,?2,size_bytes FROM output_pruning_progress WHERE attempt_id=?1", params![attempt,now])?;
            tx.execute(
                "DELETE FROM output_pruning_progress WHERE attempt_id=?1",
                [attempt],
            )?;
        }
        recorded.attempts.insert(attempt);
        recorded.bytes = recorded.bytes.saturating_add(bytes);
        recorded.budget -= removed;
    }
    Ok(())
}

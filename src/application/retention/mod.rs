//! Project-scoped retention commands. Each batch records its intent in one short write
//! transaction, deletes files outside it and records the result in another; no transaction
//! spans a file-system operation.
//!
//! - [`policy`]: retention policy, limits and capacity reserves.
//! - [`admission`]: shared capacity claims, probes and reconciliation.
//! - [`measure`]: physical allocation through one resumable tree walker.
//! - [`candidates`]: what cleanup may delete and why the rest is protected.
//! - [`deletion`]: the delete-intent, unlink and mark-pruned phases of a batch.
//! - [`sweep`]: removal of empty evidence directories.
//! - [`database`]: SQLite page and WAL reclamation.
use super::*;
use crate::resource::StorageRefusal;
use serde::Serialize;
use std::fs::File;
use std::fs::TryLockError;
use std::time::Duration;

mod admission;
mod candidates;
mod capacity;
mod database;
mod deletion;
mod measure;
mod policy;
mod sweep;
#[cfg(test)]
mod tests;

use super::artifacts::CompactionProgress;
pub(super) use admission::{
    admit_capacity, bump_admission_generation, evidence_allocation_changed,
    initialize_capacity_admission, measure_known_bytes,
};
use admission::{probe_known_allocation, projected_allocation};
use candidates::PreviewMode;
pub(super) use capacity::{Standing, storage_standing};
use database::{Reclamation, free_pages};
pub(crate) use measure::ScanProgress;
use measure::{Allocation, allocation, bounded_allocation_scan};
pub use policy::Policy;
use policy::{Reserve, instance_limits};
pub(super) use policy::{policy, validate_policy, write_policy};
#[cfg(test)]
use {
    admission::{admission_generation, settle_scan},
    measure::{ScanState, scan_state},
    policy::DEFAULT_MAX_BYTES,
    rusqlite::Connection,
    sweep::sweep_empty_evidence,
};

const CLEANUP_BATCH: i64 = 64;

/// Attempts of project `?1` whose closed task no reused check refers to: `closed` may lose
/// evidence once old enough; `unreserved` also has no artifact copy in flight.
const CLOSED: &str = "WITH closed AS (SELECT a.* FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND NOT EXISTS (SELECT 1 FROM attempt_steps source JOIN attempt_steps reused ON reused.reused_from_step_id=source.id WHERE source.attempt_id=a.id)), unreserved AS (SELECT * FROM closed c WHERE NOT EXISTS (SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=c.id))";

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
    pub logical_artifact_bytes: i64,
    pub distinct_artifact_bytes: i64,
    /// Allocated bytes still held by other links when this project's eligible paths are pruned.
    pub protected_shared_file_bytes: i64,
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

/// Projected instance allocation against the work reserve: what a step launch checks.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkCapacity {
    pub allocated_bytes: i64,
    pub allocated_files: i64,
    pub effective_max_bytes: i64,
    pub effective_max_files: i64,
}

impl WorkCapacity {
    /// Why no step may launch, when the work reserve is reached.
    pub(crate) fn refusal(&self) -> Option<StorageRefusal> {
        capacity::refusal(
            (self.allocated_bytes, self.allocated_files),
            (0, 0),
            (self.effective_max_bytes, self.effective_max_files),
            || {
                format!(
                    "Resource limit: storage capacity reserve reached ({} / {} bytes, {} / {} files/directories); evidence and work retained",
                    self.allocated_bytes,
                    self.effective_max_bytes,
                    self.allocated_files,
                    self.effective_max_files,
                )
            },
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupProgress {
    Reclaimed,
    Sweeping,
    PublicationBusy,
    Protected,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HousekeepingProgress {
    pub(crate) scan: ScanProgress,
    cleanup: CleanupProgress,
}

impl HousekeepingProgress {
    fn continues(self) -> bool {
        self.scan == ScanProgress::Visited
            || matches!(
                self.cleanup,
                CleanupProgress::Reclaimed | CleanupProgress::Sweeping
            )
    }

    fn contended(self) -> bool {
        self.scan == ScanProgress::Contended || self.cleanup == CleanupProgress::PublicationBusy
    }
}

pub(crate) fn contention_backoff(contentions: &mut u32) {
    *contentions = contentions.saturating_add(1);
    std::thread::sleep(Duration::from_millis(
        25 * u64::from((*contentions).min(10)),
    ));
}

/// One cleanup batch: the receipt, and generated-path violations it left in place.
struct Batch {
    receipt: CleanupReceipt,
    skipped: Vec<String>,
}

impl Service {
    /// One checkpoint performs at most four bounded batches. The next run-loop
    /// iteration or restart resumes from the durable sweep cursor.
    pub fn automatic_housekeeping(&mut self) -> Result<Preview> {
        Ok(self.automatic_housekeeping_progress(4)?.0)
    }

    pub(crate) fn automatic_housekeeping_progress(
        &mut self,
        batches: usize,
    ) -> Result<(Preview, HousekeepingProgress)> {
        self.housekeeping_project(self.project_id, batches, true)
    }

    pub(crate) fn reconcile_storage(&mut self) -> Result<(Preview, HousekeepingProgress)> {
        self.housekeeping_project(self.project_id, 1, false)
    }

    fn housekeeping_project(
        &mut self,
        project: i64,
        batches: usize,
        compact: bool,
    ) -> Result<(Preview, HousekeepingProgress)> {
        self.recover_pins()?;
        probe_known_allocation(&self.db, project)?;
        let projected = projected_allocation(&self.db)?;
        let reclamation = Reclamation::start(self.db.connection(), self.db.state_dir())?;
        let mut reclaimed = false;
        let mut sweeping = false;
        let mut contended = false;
        for _ in 0..batches.min(4) {
            let before = self.preview_with_allocation(project, CLEANUP_BATCH, false, &projected)?;
            if before.deferred_for_live_work {
                break;
            }
            let free_before = free_pages(self.db.connection())?;
            let Some(batch) =
                self.cleanup_batch(project, &before, CLEANUP_BATCH, &projected, false)?
            else {
                contended = true;
                break;
            };
            for path in batch.skipped {
                if self.reported_evidence.insert(path.clone()) {
                    note!("storage: {path}; other evidence is still cleaned");
                }
            }
            let receipt = batch.receipt;
            reclaimed |= receipt.bytes > 0 || receipt.attempts > 0;
            sweeping |= receipt.batch_limit_reached;
            let free_after = free_pages(self.db.connection())?;
            reclaimed |= free_after < free_before;
            if !receipt.batch_limit_reached
                && receipt.bytes == 0
                && (free_after == 0 || !database::incremental(self.db.connection())?)
            {
                break;
            }
        }
        reclamation.finish(self.db.connection(), self.db.state_dir())?;
        if compact && !contended {
            match self.compact_retained_evidence() {
                CompactionProgress::Done => {}
                CompactionProgress::More => sweeping = true,
                CompactionProgress::PublicationBusy => contended = true,
            }
        }
        let scan = bounded_allocation_scan(&self.db)?;
        let after = projected_allocation(&self.db)?;
        let cleanup = match () {
            _ if contended => CleanupProgress::PublicationBusy,
            _ if reclaimed => CleanupProgress::Reclaimed,
            _ if sweeping => CleanupProgress::Sweeping,
            _ => CleanupProgress::Protected,
        };
        Ok((
            self.preview_with_allocation(project, CLEANUP_BATCH, false, &after)?,
            HousekeepingProgress { scan, cleanup },
        ))
    }

    /// Run a bounded compaction page at an idle point or under capacity pressure.
    pub(crate) fn compact_retained_evidence(&mut self) -> CompactionProgress {
        match self.compact_artifacts() {
            Ok(progress) => progress,
            Err(error) => {
                let message = format!("Cannot compact retained evidence: {error}");
                if self.reported_evidence.insert(message.clone()) {
                    note!("storage: {message}");
                }
                CompactionProgress::Done
            }
        }
    }

    /// At shared capacity pressure, try policy-eligible cleanup in every project.
    /// Check the producer's actual claim between bounded housekeeping pages.
    pub(crate) fn reclaim_projects(
        &mut self,
        wait_for_contention: bool,
        mut needs_room: impl FnMut(&mut Self, HousekeepingProgress) -> Result<bool>,
    ) -> Result<()> {
        let mut next = Some(self.project_id);
        let mut cursor = 0;
        loop {
            let project = match next.take() {
                Some(id) => id,
                None => {
                    let id: Option<i64> = self.db.connection().query_row(
                        "SELECT min(id) FROM projects WHERE id>?1 AND id<>?2",
                        params![cursor, self.project_id],
                        |row| row.get(0),
                    )?;
                    let Some(id) = id else { return Ok(()) };
                    cursor = id;
                    id
                }
            };
            let mut contentions = 0;
            loop {
                let (_, progress) = self.housekeeping_project(project, 1, true)?;
                if !needs_room(self, progress)? {
                    return Ok(());
                }
                if progress.contended() {
                    if !wait_for_contention {
                        return Ok(());
                    }
                    contention_backoff(&mut contentions);
                } else if progress.continues() {
                    contentions = 0;
                } else {
                    break;
                }
            }
        }
    }

    pub(crate) fn retry_storage<T>(
        &mut self,
        write: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<T> {
        // Archiving completed work is best effort at a contended cleanup point.
        self.retry_storage_with(false, write, |_| Ok(None::<std::convert::Infallible>))
            .map(|result| match result {
                std::ops::ControlFlow::Continue(value) => value,
                std::ops::ControlFlow::Break(never) => match never {},
            })
    }

    pub(crate) fn retry_storage_until<T, B>(
        &mut self,
        write: impl FnMut(&mut Self) -> Result<T>,
        interrupted: impl FnMut(&Self) -> Result<Option<B>>,
    ) -> Result<std::ops::ControlFlow<B, T>> {
        self.retry_storage_with(true, write, interrupted)
    }

    fn retry_storage_with<T, B>(
        &mut self,
        wait_for_contention: bool,
        mut write: impl FnMut(&mut Self) -> Result<T>,
        mut interrupted: impl FnMut(&Self) -> Result<Option<B>>,
    ) -> Result<std::ops::ControlFlow<B, T>> {
        use std::ops::ControlFlow::{Break, Continue};
        let mut result = write(self);
        let mut stop = Ok(None);
        if matches!(result, Err(Error::Capacity(_)))
            && let Err(error) = self.reclaim_projects(wait_for_contention, |service, _| {
                stop = interrupted(service);
                if !matches!(stop, Ok(None)) {
                    return Ok(false);
                }
                result = write(service);
                Ok(matches!(result, Err(Error::Capacity(_))))
            })
        {
            note!("storage: capacity cleanup deferred ({error})");
        }
        match stop? {
            Some(reason) => Ok(Break(reason)),
            None => result.map(Continue),
        }
    }

    /// The check before every worker, check and judge step: the projected allocation after
    /// a probe of the instance's fixed files. It reads no output and walks no directory.
    pub(crate) fn work_capacity(&self) -> Result<WorkCapacity> {
        probe_known_allocation(&self.db, self.project_id)?;
        let usage = projected_allocation(&self.db)?;
        let limits = instance_limits(self.db.connection())?;
        Ok(WorkCapacity {
            allocated_bytes: usage.bytes,
            allocated_files: usage.files,
            effective_max_bytes: limits.max_bytes,
            effective_max_files: limits.max_files,
        })
    }

    pub(crate) fn launch_capacity_reached(&self) -> Result<bool> {
        let usage = projected_allocation(&self.db)?;
        let limits = instance_limits(self.db.connection())?;
        Ok(Reserve::Launch.reached(usage.bytes, usage.files, limits))
    }

    pub(crate) fn projected_storage(&self) -> Result<Preview> {
        let usage = projected_allocation(&self.db)?;
        self.preview_with_allocation(self.project_id, CLEANUP_BATCH, false, &usage)
    }

    /// Whether `bytes` and `files` more would still leave the work reserve of the storage
    /// budget free.
    pub(crate) fn storage_admits(&self, bytes: u64, files: u64) -> Result<bool> {
        probe_known_allocation(&self.db, self.project_id)?;
        let usage = projected_allocation(&self.db)?;
        let limits = instance_limits(self.db.connection())?;
        Ok(!Reserve::Work.reached(
            usage
                .bytes
                .saturating_add(bytes.min(i64::MAX as u64) as i64),
            usage
                .files
                .saturating_add(files.min(i64::MAX as u64) as i64),
            limits,
        ))
    }

    /// Retire idle leftover evidence pins before, never inside, a cleanup transaction.
    fn recover_pins(&self) -> Result<()> {
        super::evidence_pin::recover(self.db.connection(), self.db.state_dir())
    }

    fn preview_with_allocation(
        &self,
        project: i64,
        candidate_limit: i64,
        accounting: bool,
        observed: &Allocation,
    ) -> Result<Preview> {
        let read = rusqlite::Transaction::new_unchecked(
            self.db.connection(),
            TransactionBehavior::Deferred,
        )?;
        let preview = candidates::preview(
            &read,
            self.db.state_dir(),
            project,
            now_ms(),
            observed,
            PreviewMode {
                candidate_limit,
                accounting,
            },
        )?;
        read.commit()?;
        Ok(self.with_vacuum_failure(preview))
    }

    pub fn storage_preview(&self) -> Result<Preview> {
        self.recover_pins()?;
        let observed = allocation(self.db.state_dir())?;
        self.preview_with_allocation(self.project_id, i64::MAX, true, &observed)
    }

    pub fn cleanup_storage(&mut self, expected: &Preview) -> Result<CleanupReceipt> {
        let reclamation = Reclamation::start(self.db.connection(), self.db.state_dir())?;
        let observed = allocation(self.db.state_dir())?;
        let batch = self
            .cleanup_batch(self.project_id, expected, i64::MAX, &observed, true)?
            .expect("interactive cleanup waits for the publication lock");
        reclamation.finish(self.db.connection(), self.db.state_dir())?;
        if !batch.skipped.is_empty() {
            return Err(Error::Conflict(format!(
                "{}; cleanup stopped",
                batch.skipped.join("; ")
            )));
        }
        Ok(batch.receipt)
    }

    /// Delete one batch of eligible evidence: artifact files, then output rows, then empty
    /// directories while the batch budget lasts. A generated-path violation is skipped and
    /// returned; an interactive batch stops before the directory sweep when one occurs.
    fn cleanup_batch(
        &mut self,
        project: i64,
        expected: &Preview,
        candidate_limit: i64,
        observed: &Allocation,
        interactive: bool,
    ) -> Result<Option<Batch>> {
        let root = self.db.state_dir().try_clone()?;
        let publication = super::artifacts::publication_lock(&self.db.state_file("")?)?;
        if interactive {
            publication.lock()?;
        } else {
            match publication.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        self.recover_pins()?;
        let mode = PreviewMode {
            candidate_limit,
            accounting: false,
        };
        let mut intent = deletion::record_intent(
            self.db.connection(),
            &root,
            project,
            expected,
            observed,
            mode,
        )?;
        let (mut recorded, unlinked) = match intent.recorded.take() {
            Some(recorded) => (recorded, deletion::Unlinked::default()),
            None => {
                let unlinked = deletion::unlink(&root, &intent.selected);
                let recorded = deletion::record_deletion(
                    self.db.connection(),
                    &root,
                    project,
                    &intent,
                    &unlinked,
                    observed,
                    mode,
                )?;
                (recorded, unlinked)
            }
        };
        if let Some(error) = unlinked.failure {
            return Err(error);
        }
        if recorded.budget > 0 && (!interactive || unlinked.skipped.is_empty()) {
            let visited = sweep::sweep_batch(
                self.db.connection(),
                &root,
                project,
                &mut recorded.budget,
                recorded.pressure,
                interactive,
            )?;
            recorded.attempts.extend(visited);
        }
        Ok(Some(Batch {
            receipt: CleanupReceipt {
                attempts: recorded.attempts.len(),
                bytes: recorded.bytes,
                batch_limit_reached: recorded.budget == 0,
            },
            skipped: unlinked.skipped,
        }))
    }
}

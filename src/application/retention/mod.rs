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
use serde::Serialize;
use std::fs::File;

mod admission;
mod candidates;
mod database;
mod deletion;
mod measure;
mod policy;
mod sweep;
#[cfg(test)]
mod tests;

pub(super) use admission::{
    admit_capacity, bump_admission_generation, initialize_capacity_admission, measure_known_bytes,
};
use admission::{probe_known_allocation, projected_allocation};
use candidates::PreviewMode;
use database::{Reclamation, free_pages};
use measure::{Allocation, ScanProgress, allocation, bounded_allocation_scan};
pub use policy::Policy;
use policy::{Reserve, instance_limits};
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
    pub reached: bool,
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
        self.recover_pins()?;
        probe_known_allocation(&self.db, self.project_id)?;
        let projected = projected_allocation(&self.db)?;
        let reclamation = Reclamation::start(self.db.connection(), self.db.state_dir())?;
        let mut reclaimed = false;
        let mut sweeping = false;
        for _ in 0..batches.min(4) {
            let before = self.preview_with_allocation(CLEANUP_BATCH, false, &projected)?;
            if before.deferred_for_live_work {
                break;
            }
            let free_before = free_pages(self.db.connection())?;
            let batch = self.cleanup_batch(&before, CLEANUP_BATCH, &projected, false)?;
            for path in batch.skipped {
                if self.reported_evidence.insert(path.clone()) {
                    eprintln!("storage: {path}; other evidence is still cleaned");
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
        let scan = bounded_allocation_scan(&self.db)?;
        let after = projected_allocation(&self.db)?;
        let progress = match scan {
            ScanProgress::Visited => HousekeepingProgress::Reconciling,
            ScanProgress::Contended => HousekeepingProgress::Contended,
            ScanProgress::Settled if reclaimed => HousekeepingProgress::Reclaimed,
            ScanProgress::Settled if sweeping => HousekeepingProgress::Sweeping,
            ScanProgress::Settled => HousekeepingProgress::Protected,
        };
        Ok((
            self.preview_with_allocation(CLEANUP_BATCH, false, &after)?,
            progress,
        ))
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
            reached: Reserve::Work.reached(usage.bytes, usage.files, limits),
        })
    }

    /// Retire idle leftover evidence pins before, never inside, a cleanup transaction.
    fn recover_pins(&self) -> Result<()> {
        super::evidence_pin::recover(self.db.connection(), self.db.state_dir())
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
        let preview = candidates::preview(
            &read,
            self.db.state_dir(),
            self.project_id,
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
        self.preview_with_allocation(i64::MAX, true, &observed)
    }

    pub fn cleanup_storage(&mut self, expected: &Preview) -> Result<CleanupReceipt> {
        let reclamation = Reclamation::start(self.db.connection(), self.db.state_dir())?;
        let observed = allocation(self.db.state_dir())?;
        let batch = self.cleanup_batch(expected, i64::MAX, &observed, true)?;
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
        expected: &Preview,
        candidate_limit: i64,
        observed: &Allocation,
        interactive: bool,
    ) -> Result<Batch> {
        let root = self.db.state_dir().try_clone()?;
        self.recover_pins()?;
        let mode = PreviewMode {
            candidate_limit,
            accounting: false,
        };
        let mut intent = deletion::record_intent(
            self.db.connection(),
            &root,
            self.project_id,
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
                    self.project_id,
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
                self.project_id,
                &mut recorded.budget,
                recorded.pressure,
                interactive,
            )?;
            recorded.attempts.extend(visited);
        }
        Ok(Batch {
            receipt: CleanupReceipt {
                attempts: recorded.attempts.len(),
                bytes: recorded.bytes,
                batch_limit_reached: recorded.budget == 0,
            },
            skipped: unlinked.skipped,
        })
    }
}

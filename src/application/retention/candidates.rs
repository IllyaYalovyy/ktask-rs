//! What cleanup may delete, what it may not and why: the storage preview.
use super::database::vacuum_note;
use super::measure::Allocation;
use super::policy::{Reserve, instance_limits, policy};
use super::*;
use crate::application::evidence_path::EvidencePath;
use rusqlite::Connection;

/// Logical output size of attempt `a`; counters exist for every attempt once opened.
pub(super) const OUTPUT_BYTES: &str = "coalesce((SELECT bytes FROM attempt_output_usage WHERE attempt_id=a.id),(SELECT sum(length(CAST(content AS BLOB))) FROM attempt_output WHERE attempt_id=a.id),0)";
const OUTPUT_ROWS: &str = "coalesce((SELECT rows FROM attempt_output_usage WHERE attempt_id=a.id),(SELECT count(*) FROM attempt_output WHERE attempt_id=a.id))";
const ARTIFACT_BYTES: &str = "coalesce((SELECT sum(size_bytes) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),0)";

#[derive(Clone, Copy)]
pub(super) struct PreviewMode {
    pub candidate_limit: i64,
    /// Sum the project's logical usage and the eligible files' allocation: interactive only.
    pub accounting: bool,
}

/// Logical evidence bytes the minimum age, reuse references and open work keep, so the owner
/// can see why a budget cannot be met by cleanup.
fn protection_breakdown(db: &Connection, project: i64, cutoff: i64) -> Result<(i64, i64, i64)> {
    Ok(db.query_row(
        &format!(
            "{CLOSED} SELECT
            coalesce(sum(CASE WHEN eligible AND finished_at_ms>?2 THEN b ELSE 0 END),0),
            coalesce(sum(CASE WHEN closed_task AND NOT eligible THEN b ELSE 0 END),0),
            coalesce(sum(CASE WHEN NOT closed_task THEN b ELSE 0 END),0)
            FROM (SELECT a.finished_at_ms, t.closed_at_ms IS NOT NULL AS closed_task,
                a.id IN (SELECT id FROM closed) AS eligible,
                {OUTPUT_BYTES} + {ARTIFACT_BYTES} AS b
                FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1)"
        ),
        params![project, cutoff],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// Judges and workers can refer to earlier transcripts. Defer the whole project while any
/// attempt, project-level provider call or evidence pin is running. Paused and waiting
/// attempts retain their own evidence, but unrelated closed attempts can still be cleaned.
/// A call inside an attempt is live only through its attempt, so a call record left behind
/// by a crash cannot defer cleanup forever.
pub(super) fn protected(db: &Connection, project: i64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state='running' UNION ALL SELECT 1 FROM model_calls WHERE project_id=?1 AND step_id IS NULL AND state='running' UNION ALL SELECT 1 FROM evidence_pins WHERE project_id=?1)", [project], |r| r.get(0))?)
}

fn resumable(db: &Connection, project: i64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('paused','waiting'))", [project], |r| r.get(0))?)
}

/// The minimum age is a hard floor: capacity pressure never widens eligibility.
fn candidates(db: &Connection, project: i64, cutoff: i64, limit: i64) -> Result<Vec<Candidate>> {
    let mut stmt = db.prepare(&format!("{CLOSED} SELECT a.task_id,a.id,{OUTPUT_BYTES},{OUTPUT_ROWS},{ARTIFACT_BYTES},
        (SELECT count(*) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),
        (SELECT count(*) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NOT NULL)
        FROM closed a
        WHERE (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruning_requested_at_ms IS NOT NULL AND pruned_at_ms IS NULL)
               OR EXISTS(SELECT 1 FROM output_pruning_progress WHERE attempt_id=a.id))
          AND (EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=a.id)
               OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL))
        ORDER BY a.id LIMIT ?3"))?;
    Ok(stmt
        .query_map(params![project, cutoff, limit], |r| {
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
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Allocated bytes of the candidates' generated files, which cleanup would delete.
fn eligible_file_bytes(db: &Connection, root: &File, candidates: &[Candidate]) -> Result<i64> {
    let mut files = db.prepare(
        "SELECT relative_path FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL",
    )?;
    let mut total = 0_i64;
    for candidate in candidates {
        for path in files.query_map([candidate.attempt_id], |r| r.get::<_, String>(0))? {
            let path = path?;
            if let Some(evidence) = EvidencePath::parse(&path)
                .filter(|evidence| evidence.attempt == candidate.attempt_id && evidence.generated())
            {
                total = total.saturating_add(super::deletion::allocated(root, &evidence));
            }
        }
    }
    Ok(total)
}

pub(super) fn preview(
    db: &Connection,
    root: &File,
    project: i64,
    now: i64,
    allocation: &Allocation,
    mode: PreviewMode,
) -> Result<Preview> {
    let policy = policy(db, project)?;
    let limits = instance_limits(db)?;
    let (output, artifacts): (i64, i64) = if mode.accounting {
        (db.query_row("SELECT coalesce(sum(u.bytes),0) FROM attempt_output_usage u JOIN attempts a ON a.id=u.attempt_id WHERE a.project_id=?1", [project], |r| r.get(0))?,
        db.query_row("SELECT coalesce(sum(f.size_bytes),0) FROM artifacts f JOIN attempts a ON a.id=f.attempt_id WHERE a.project_id=?1 AND f.pruned_at_ms IS NULL", [project], |r| r.get(0))?)
    } else {
        (0, 0)
    };
    let live = protected(db, project)?;
    let cutoff = policy.cutoff(now);
    let candidates = if live {
        Vec::new()
    } else {
        candidates(db, project, cutoff, mode.candidate_limit)?
    };
    let reclaimable_bytes = candidates
        .iter()
        .map(|c| c.output_bytes.saturating_add(c.artifact_bytes))
        .fold(0_i64, i64::saturating_add);
    let eligible_file_bytes = if mode.accounting {
        eligible_file_bytes(db, root, &candidates)?
    } else {
        0
    };
    let reusable_database_bytes = super::database::free_pages(db)?
        .saturating_mul(db.pragma_query_value(None, "page_size", |r| r.get::<_, i64>(0))?)
        .min(allocation.database);
    let deferred_for_live_work = live || (candidates.is_empty() && resumable(db, project)?);
    let launch_capacity_reached =
        Reserve::Launch.reached(allocation.bytes, allocation.files, limits);
    let (protected_by_age_bytes, protected_by_reference_bytes, protected_open_bytes) =
        if mode.accounting || launch_capacity_reached {
            protection_breakdown(db, project, cutoff)?
        } else {
            (0, 0, 0)
        };
    Ok(Preview {
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
        vacuum_note: vacuum_note(db, root)?,
        over_capacity: allocation.bytes >= limits.max_bytes || allocation.files >= limits.max_files,
        work_capacity_reached: Reserve::Work.reached(allocation.bytes, allocation.files, limits),
        launch_capacity_reached,
        effective_max_bytes: limits.max_bytes,
        effective_max_files: limits.max_files,
        candidates,
        deferred_for_live_work,
    })
}

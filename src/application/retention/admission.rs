//! Instance-wide capacity admission. Producers claim an estimate of their physical growth
//! in their own write transaction; probes charge growth of the fixed files; a settled
//! allocation scan replaces the projection with a measurement plus the claims admitted
//! while it ran.
use super::measure::{Allocation, Walk, known_allocation, project_runner_logs};
use super::policy::{Reserve, instance_limits};
use super::*;
use rusqlite::Connection;

const SQLITE_PAGE: u64 = 4096;
/// Usable leaf bytes before a cell spills to overflow pages.
const SQLITE_LEAF: u64 = SQLITE_PAGE - 35;
/// Row header, rowid and the (attempt, sequence) primary-key index entry.
const OUTPUT_ROW_OVERHEAD: u64 = 40;
/// Evidence, attempt and step directories plus the artifact's database rows.
const ARTIFACT_FILE_OVERHEAD: u64 = 4 * SQLITE_PAGE;
/// Directory entries one artifact may add: its file and up to three parents.
const ARTIFACT_FILE_ENTRIES: i64 = 4;
/// Growth of the fixed files below this is one housekeeping WAL page, not a charge.
const PROBE_THRESHOLD: i64 = 64 * 1024;

/// Physical database bytes for `rows` output rows holding `bytes` of text.
/// Rows pack whole into pages, so a row over half a page wastes the rest of
/// its page; larger rows spill into whole overflow pages.
fn output_claim(bytes: u64, rows: u64) -> u64 {
    if rows == 0 {
        return bytes;
    }
    let cell = (bytes / rows).saturating_add(OUTPUT_ROW_OVERHEAD);
    let per_row = if cell > SQLITE_LEAF {
        cell.div_ceil(SQLITE_PAGE - 4).saturating_mul(SQLITE_PAGE)
    } else {
        SQLITE_PAGE / (SQLITE_LEAF / cell).max(1)
    };
    per_row.saturating_mul(rows).max(bytes)
}

/// Physical bytes of artifact content (whole blocks) and its directories.
fn artifact_claim(bytes: u64, files: u64) -> u64 {
    bytes
        .div_ceil(SQLITE_PAGE)
        .saturating_mul(SQLITE_PAGE)
        .saturating_add(files.saturating_mul(ARTIFACT_FILE_OVERHEAD))
}

fn clamp(bytes: u64) -> i64 {
    bytes.min(i64::MAX as u64) as i64
}

/// Initialize the instance-wide admission watermark outside the database write lock.
/// Concurrent opens may measure the same instance; only the first inserts its row.
pub(in crate::application) fn initialize_capacity_admission(db: &Database) -> Result<()> {
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

/// Output rows and artifact reservations claim an estimate of their physical size in
/// their SQLite write transaction, serialized across projects in one instance.
pub(in crate::application) fn admit_capacity(
    tx: &Connection,
    measured: i64,
    output_bytes: u64,
    output_rows: u64,
    artifact_bytes: u64,
    artifact_files: u64,
) -> Result<()> {
    let limits = instance_limits(tx)?;
    let (projected, projected_files, frontier): (i64, i64, i64) = tx.query_row(
        "SELECT projected_bytes,projected_files,known_bytes FROM instance_storage_admission WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    // The claim approximates the physical bytes the write will occupy. The
    // known-allocation frontier is the measured size plus claims not yet
    // visible on disk; only physical growth beyond it (WAL frames, page
    // overhead) is charged again, so a claim is never counted twice.
    let output = clamp(output_claim(output_bytes, output_rows));
    let excess = measured.saturating_sub(frontier).max(0);
    let projected_bytes = projected.saturating_add(excess);
    let claim_bytes =
        clamp((output as u64).saturating_add(artifact_claim(artifact_bytes, artifact_files)));
    let claim_files = clamp(artifact_files).saturating_mul(ARTIFACT_FILE_ENTRIES);
    if Reserve::Work.reached(
        projected_bytes.saturating_add(claim_bytes),
        projected_files.saturating_add(claim_files),
        limits,
    ) {
        return Err(Error::Conflict(format!(
            "Resource limit: shared storage capacity reserve reached (projected {projected_bytes} / {} bytes, {projected_files} / {} files); accepted evidence retained; raise capacity or reclaim closed evidence before resuming",
            limits.max_bytes, limits.max_files
        )));
    }
    tx.execute(
        "UPDATE instance_storage_admission SET projected_bytes=?1,projected_files=projected_files+?2,known_bytes=?3,generation=generation+1 WHERE id=1",
        params![
            projected_bytes.saturating_add(claim_bytes),
            claim_files,
            measured.max(frontier).saturating_add(output)
        ],
    )?;
    Ok(())
}

/// Cheap physical measure for [`admit_capacity`]: a few `fstat` calls.
pub(in crate::application) fn measure_known_bytes(db: &Database) -> Result<i64> {
    Ok(known_allocation(db.state_dir())?.bytes)
}

pub(super) fn admission_generation(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT generation FROM instance_storage_admission WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

/// The settled measurement no longer describes the instance; the next scan replaces it.
pub(in crate::application) fn bump_admission_generation(db: &Connection) -> Result<()> {
    db.execute(
        "UPDATE instance_storage_admission SET generation=generation+1 WHERE id=1",
        [],
    )?;
    Ok(())
}

/// Reclamation changes physical allocation without an admission. A lost invalidation
/// leaves the larger, conservative projection.
pub(super) fn invalidate_known_allocation(db: &Connection, root: &File) -> Result<()> {
    let known = known_allocation(root)?;
    super::database::unsynced(db, || {
        db.execute("UPDATE instance_storage_admission SET known_bytes=?1,known_files=?2,generation=generation+1 WHERE id=1", params![known.bytes,known.files])?;
        Ok(())
    })
}

/// The projection's base when a scan starts: the claims of reservations still copying
/// are subtracted, because the walk may miss their files and the settled projection
/// must keep them.
pub(super) fn scan_base(tx: &Connection) -> Result<(i64, i64, i64)> {
    let (generation, projected_bytes, projected_files): (i64, i64, i64) = tx.query_row(
        "SELECT generation,projected_bytes,projected_files FROM instance_storage_admission WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let (pending_bytes, pending_files): (i64, i64) = tx.query_row(
        "SELECT coalesce(sum(size_bytes),0),count(*) FROM artifact_reservations",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let pending = clamp(artifact_claim(
        pending_bytes.max(0) as u64,
        pending_files.max(0) as u64,
    ));
    Ok((
        generation,
        projected_bytes.saturating_sub(pending),
        projected_files.saturating_sub(pending_files.saturating_mul(ARTIFACT_FILE_ENTRIES)),
    ))
}

/// Install a completed walk. Claims admitted while it ran, and those of reservations
/// pending when it started, are added to the measurement instead of discarding it, so a
/// busy instance still settles. Only the scan's own revision may settle it; if anything
/// changed meanwhile, the next housekeeping call measures again.
pub(super) fn settle_scan(db: &Database, revision: i64, walk: &Walk) -> Result<bool> {
    let known = known_allocation(db.state_dir())?;
    super::database::unsynced(db.connection(), || {
        let tx =
            rusqlite::Transaction::new_unchecked(db.connection(), TransactionBehavior::Immediate)?;
        let scan: Option<(i64, i64, i64)> = tx
        .query_row(
            "SELECT generation,base_bytes,base_files FROM instance_allocation_scan WHERE id=1 AND revision=?1 AND complete=1",
            [revision],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
        let Some((started, base_bytes, base_files)) = scan else {
            return Ok(false);
        };
        let (generation, projected_bytes, projected_files): (i64, i64, i64) = tx.query_row(
        "SELECT generation,projected_bytes,projected_files FROM instance_storage_admission WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
        let observed = super::measure::with_sqlite(db.state_dir(), walk.usage)?;
        let bytes = observed
            .bytes
            .saturating_add(projected_bytes.saturating_sub(base_bytes).max(0));
        let files = observed
            .files
            .saturating_add(projected_files.saturating_sub(base_files).max(0));
        tx.execute(
        "UPDATE instance_storage_admission SET projected_bytes=?1,projected_files=?2,known_bytes=?3,known_files=?4,generation=generation+1 WHERE id=1",
        params![bytes, files, known.bytes, known.files],
    )?;
        // An unchanged instance is settled at the new generation; otherwise the old one
        // differs from it and the next call starts a fresh walk.
        let settled = if generation == started {
            generation + 1
        } else {
            started
        };
        tx.execute(
        "UPDATE instance_allocation_scan SET generation=?1,complete=2,revision=revision+1,bytes=?2,files=?3,database_bytes=?4,wal_bytes=?5 WHERE id=1",
        params![settled, observed.bytes, observed.files, observed.database, observed.wal],
    )?;
        tx.commit()?;
        Ok(true)
    })
}

pub(super) fn projected_allocation(db: &Database) -> Result<Allocation> {
    Ok(db.connection().query_row(
        "SELECT a.projected_bytes,a.projected_files,coalesce(s.database_bytes,0),coalesce(s.wal_bytes,0),coalesce(s.runner_log_bytes,0),coalesce(s.evidence_bytes,0),coalesce(s.evidence_entries,0),coalesce(s.directory_entries,0) FROM instance_storage_admission a LEFT JOIN instance_allocation_scan s ON s.id=1 AND s.generation=a.generation AND s.complete=2 WHERE a.id=1",
        [],
        |r| Ok(Allocation { bytes:r.get(0)?,files:r.get(1)?,database:r.get(2)?,wal:r.get(3)?,runner_logs:r.get(4)?,evidence:r.get(5)?,evidence_entries:r.get(6)?,directories:r.get(7)? }),
    )?)
}

/// Charge growth of the instance's fixed files and this project's runner logs since the
/// last probe. Small changes accumulate against the last observed baseline.
pub(super) fn probe_known_allocation(db: &Database, project: i64) -> Result<()> {
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
    // Ignore a single housekeeping WAL page; the admission reserve covers this margin.
    if gained_bytes >= PROBE_THRESHOLD || gained_files > 0 || log_growth > 0 || log_files > 0 {
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

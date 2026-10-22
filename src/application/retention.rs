//! Project-scoped retention commands. The database write lock covers selection and deletion.
use super::*;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::Mode,
    unistd::{UnlinkatFlags, unlinkat},
};
use rusqlite::Connection;
use serde::Serialize;
use std::fs::File;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct Policy {
    pub days: i64,
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
    pub candidates: Vec<Candidate>,
    pub deferred_for_live_work: bool,
}

fn policy(db: &Connection, project: i64) -> Result<Policy> {
    Ok(db
        .query_row(
            "SELECT days,version FROM project_retention WHERE project_id=?1",
            [project],
            |r| {
                Ok(Policy {
                    days: r.get(0)?,
                    version: r.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or(Policy {
            days: 30,
            version: 0,
        }))
}

fn preview(
    db: &Connection,
    root: &File,
    project: i64,
    now: i64,
) -> Result<(Preview, super::evidence_pin::Recovery)> {
    let recovered = super::evidence_pin::recover(db, root)?;
    let policy = policy(db, project)?;
    let output: i64 = db.query_row("SELECT coalesce(sum(length(CAST(o.content AS BLOB))),0) FROM attempt_output o JOIN attempts a ON a.id=o.attempt_id WHERE a.project_id=?1", [project], |r| r.get(0))?;
    let artifacts: i64 = db.query_row("SELECT coalesce(sum(f.size_bytes),0) FROM artifacts f JOIN attempts a ON a.id=f.attempt_id WHERE a.project_id=?1 AND f.pruned_at_ms IS NULL", [project], |r| r.get(0))?;
    // Judges and workers can refer to earlier transcripts. Defer the whole project while any
    // attempt, project-level provider call or evidence pin is live; the write transaction
    // rechecks. A call inside an attempt is live only through its attempt, so a call record
    // left behind by a crash cannot defer cleanup forever.
    let live: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('running','paused','waiting') UNION ALL SELECT 1 FROM model_calls WHERE project_id=?1 AND step_id IS NULL AND state='running' UNION ALL SELECT 1 FROM evidence_pins WHERE project_id=?1)", [project], |r| r.get(0))?;
    let mut candidates = Vec::new();
    if !live {
        let cutoff = now.saturating_sub(policy.days.saturating_mul(86_400_000));
        let mut stmt = db.prepare("SELECT a.task_id,a.id,
            coalesce((SELECT sum(length(CAST(content AS BLOB))) FROM attempt_output WHERE attempt_id=a.id),0),
            (SELECT count(*) FROM attempt_output WHERE attempt_id=a.id),
            coalesce((SELECT sum(size_bytes) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),0),
            (SELECT count(*) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL),
            (SELECT count(*) FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NOT NULL)
            FROM attempts a JOIN tasks t ON t.id=a.task_id
            WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL
              AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruning_requested_at_ms IS NOT NULL AND pruned_at_ms IS NULL))
              AND (EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=a.id)
                   OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruned_at_ms IS NULL))
            ORDER BY a.id")?;
        candidates = stmt
            .query_map(params![project, cutoff], |r| {
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
    Ok((
        Preview {
            policy,
            usage_bytes: output.saturating_add(artifacts),
            reclaimable_bytes,
            candidates,
            deferred_for_live_work: live,
        },
        recovered,
    ))
}

fn io(error: Errno) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(error as i32))
}

/// Only generated transcript paths are eligible. Directory descriptors with
/// O_NOFOLLOW prevent a swapped symlink from redirecting deletion into a project.
fn remove_transcript(root: &File, attempt: i64, relative: &str) -> Result<()> {
    let expected_dir = format!("attempt-{attempt}");
    let parts: Vec<_> = relative.split('/').collect();
    require(
        parts.len() == 3
            && parts[0] == "evidence"
            && parts[1] == expected_dir
            && parts[2].starts_with("step-")
            && parts[2].ends_with(".log")
            && parts[2][5..parts[2].len() - 4].parse::<u64>().is_ok(),
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
    match unlinkat(&directory, parts[2], UnlinkatFlags::NoRemoveDir) {
        Ok(()) => File::from(directory).sync_all().map_err(Into::into),
        Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(io(e)),
    }
}

impl Service {
    pub fn retention_policy(&self) -> Result<Policy> {
        policy(self.db.connection(), self.project_id)
    }

    pub fn set_retention(&mut self, expected: Policy, days: i64) -> Result<Policy> {
        require(
            (0..=365000).contains(&days),
            "Retention days must be between 0 and 365000",
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
            version: expected.version + 1,
        };
        tx.execute("INSERT INTO project_retention(project_id,days,version) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET days=excluded.days,version=excluded.version", params![self.project_id, days, next.version])?;
        tx.commit()?;
        Ok(next)
    }

    pub fn storage_preview(&self) -> Result<Preview> {
        let tx = rusqlite::Transaction::new_unchecked(
            self.db.connection(),
            TransactionBehavior::Immediate,
        )?;
        let (preview, recovered) = preview(&tx, self.db.state_dir(), self.project_id, now_ms())?;
        tx.commit()?;
        recovered.finish()?;
        Ok(preview)
    }

    pub fn cleanup_storage(&mut self, expected: &Preview) -> Result<Preview> {
        let root = self.db.state_dir().try_clone()?;
        let intent = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, recovered) = preview(&intent, &root, self.project_id, now_ms())?;
        require(
            current.policy == expected.policy && current.candidates == expected.candidates,
            "Storage changed since preview; preview again",
        )?;
        for candidate in &current.candidates {
            intent.execute("UPDATE artifacts SET pruning_requested_at_ms=max(created_at_ms,?2) WHERE attempt_id=?1 AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NULL", params![candidate.attempt_id,now_ms()])?;
        }
        intent.commit()?;
        recovered.finish()?;
        // If deletion or its metadata commit is interrupted, this durable intent
        // makes the missing file an explicit, retryable cleanup operation.
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (rechecked, recovered) = preview(&tx, &root, self.project_id, now_ms())?;
        require(
            !rechecked.deferred_for_live_work
                && rechecked.policy == expected.policy
                && current.candidates.iter().all(|before| {
                    rechecked.candidates.iter().any(|after| {
                        before.task_id == after.task_id
                            && before.attempt_id == after.attempt_id
                            && before.output_bytes == after.output_bytes
                            && before.output_records == after.output_records
                            && before.artifact_bytes == after.artifact_bytes
                            && before.artifacts == after.artifacts
                    })
                }),
            "Storage or active work changed during cleanup; preview again",
        )?;
        let mut failure = None;
        'attempts: for candidate in &current.candidates {
            let mut stmt = tx.prepare("SELECT id,relative_path FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NOT NULL ORDER BY id")?;
            let files = stmt
                .query_map([candidate.attempt_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, path) in files {
                if let Err(error) = remove_transcript(&root, candidate.attempt_id, &path) {
                    failure = Some(error);
                    break 'attempts;
                }
                tx.execute("UPDATE artifacts SET pruned_at_ms=max(created_at_ms,?2),pruning_requested_at_ms=NULL WHERE id=?1 AND pruned_at_ms IS NULL", params![id, now_ms()])?;
            }
            if candidate.output_records > 0 {
                tx.execute("INSERT OR IGNORE INTO output_pruning(attempt_id,pruned_at_ms,size_bytes) VALUES (?1,?2,?3)", params![candidate.attempt_id,now_ms(),candidate.output_bytes])?;
                tx.execute(
                    "DELETE FROM attempt_output WHERE attempt_id=?1",
                    [candidate.attempt_id],
                )?;
            }
        }
        tx.commit()?;
        recovered.finish()?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(current)
    }
}

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

const CLEANUP_BATCH: i64 = 64;

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

#[derive(Clone, Debug, Serialize)]
pub struct CleanupReceipt {
    pub attempts: usize,
    pub bytes: i64,
    pub batch_limit_reached: bool,
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
    let live = protected(db, project)?;
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
              AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM artifacts WHERE attempt_id=a.id AND pruning_requested_at_ms IS NOT NULL AND pruned_at_ms IS NULL)
                   OR EXISTS(SELECT 1 FROM output_pruning_progress WHERE attempt_id=a.id))
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

fn protected(db: &Connection, project: i64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('running','paused','waiting') UNION ALL SELECT 1 FROM model_calls WHERE project_id=?1 AND step_id IS NULL AND state='running' UNION ALL SELECT 1 FROM evidence_pins WHERE project_id=?1)", [project], |r| r.get(0))?)
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
}

/// A durable cursor limits both the number of attempts and the number of rows
/// within a large attempt. Only recorded steps and artifact parents are owned.
fn sweep_empty_evidence(
    db: &Connection,
    root: &File,
    project: i64,
    budget: &mut i64,
) -> Result<Vec<i64>> {
    require(
        !protected(db, project)?,
        "Storage or active work changed during cleanup; preview again",
    )?;
    let retention = policy(db, project)?;
    let cutoff = now_ms().saturating_sub(retention.days.saturating_mul(86_400_000));
    let mut cursor = db.query_row("SELECT last_attempt_id,active_attempt_id,last_step_id,last_artifact_id,dirty FROM evidence_sweep WHERE project_id=?1", [project], |r| Ok(SweepCursor {last_attempt:r.get(0)?,active_attempt:r.get(1)?,last_step:r.get(2)?,last_artifact:r.get(3)?,dirty:r.get(4)?})).optional()?.unwrap_or_default();
    if cursor.dirty {
        cursor = SweepCursor::default();
    }
    let mut visited = Vec::new();
    while *budget > 0 {
        let attempt: Option<i64> = db.query_row("SELECT a.id FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM evidence_cleanup_admissions d WHERE d.attempt_id=a.id)) AND a.id=?3 AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id)", params![project,cutoff,cursor.active_attempt], |r| r.get(0)).optional()?;
        let attempt = if let Some(id) = attempt {
            id
        } else {
            cursor.active_attempt = 0;
            cursor.last_step = 0;
            cursor.last_artifact = 0;
            let next: Option<i64> = db.query_row("SELECT a.id FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM evidence_cleanup_admissions d WHERE d.attempt_id=a.id)) AND a.id>?3 AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id) ORDER BY a.id LIMIT 1", params![project,cutoff,cursor.last_attempt], |r| r.get(0)).optional()?;
            let Some(id) = next else {
                cursor.last_attempt = 0;
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
    db.execute("INSERT INTO evidence_sweep(project_id,last_attempt_id,active_attempt_id,last_step_id,last_artifact_id,dirty) VALUES (?1,?2,?3,?4,?5,0) ON CONFLICT(project_id) DO UPDATE SET last_attempt_id=excluded.last_attempt_id,active_attempt_id=excluded.active_attempt_id,last_step_id=excluded.last_step_id,last_artifact_id=excluded.last_artifact_id,dirty=0", params![project,cursor.last_attempt,cursor.active_attempt,cursor.last_step,cursor.last_artifact])?;
    Ok(visited)
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

    pub fn cleanup_storage(&mut self, expected: &Preview) -> Result<CleanupReceipt> {
        let root = self.db.state_dir().try_clone()?;
        let mut budget = CLEANUP_BATCH;
        let intent = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, recovered) = preview(&intent, &root, self.project_id, now_ms())?;
        require(
            !current.deferred_for_live_work
                && current.policy == expected.policy
                && current.candidates == expected.candidates,
            "Storage changed since preview; preview again",
        )?;
        let cutoff = now_ms().saturating_sub(current.policy.days.saturating_mul(86_400_000));
        let selected = intent.prepare("SELECT f.id,f.attempt_id,f.relative_path,f.size_bytes FROM artifacts f JOIN attempts a ON a.id=f.attempt_id JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND f.pruned_at_ms IS NULL AND (a.finished_at_ms<=?2 OR f.pruning_requested_at_ms IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id) ORDER BY f.id LIMIT ?3")?.query_map(params![self.project_id,cutoff,CLEANUP_BATCH], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
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
        let (rechecked, recovered) = preview(&tx, &root, self.project_id, now_ms())?;
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
            let rows = tx.prepare("SELECT o.attempt_id,o.sequence,length(CAST(o.content AS BLOB)) FROM attempt_output o JOIN attempts a ON a.id=o.attempt_id JOIN tasks t ON t.id=a.task_id WHERE a.project_id=?1 AND t.closed_at_ms IS NOT NULL AND (a.finished_at_ms<=?2 OR EXISTS(SELECT 1 FROM output_pruning_progress WHERE attempt_id=a.id)) AND NOT EXISTS(SELECT 1 FROM artifact_reservations r WHERE r.attempt_id=a.id) ORDER BY o.attempt_id,o.sequence LIMIT ?3")?.query_map(params![self.project_id,cutoff,budget], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
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
            let visited = sweep_empty_evidence(&tx, &root, self.project_id, &mut budget)?;
            processed.extend(visited);
            tx.commit()?;
            return Ok(CleanupReceipt {
                attempts: processed.len(),
                bytes: processed_bytes,
                batch_limit_reached: budget == 0,
            });
        }
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
            sweep_empty_evidence(&tx, service.db.state_dir(), service.project_id, &mut budget)
                .is_err()
        );
        tx.rollback()?;
        assert!(directory.exists());
        other.connection().execute("UPDATE model_calls SET state='succeeded',finished_at_ms=2 WHERE request_key='guard-call'", [])?;
        let tx =
            Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
        let mut budget = CLEANUP_BATCH;
        sweep_empty_evidence(&tx, service.db.state_dir(), service.project_id, &mut budget)?;
        tx.commit()?;
        assert!(!directory.exists());
        Ok(())
    }
}

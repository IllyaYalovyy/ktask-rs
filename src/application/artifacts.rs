//! Atomic admission of deliberate evidence. A reservation records the budget and
//! provenance before bytes enter state storage; opening the project reconciles an
//! interrupted copy before exposing it to readers or retention.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions, TryLockError},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const FILE_LIMIT: u64 = 8 * 1024 * 1024;
const ATTEMPT_LIMIT: u64 = 64 * 1024 * 1024;

fn media_type(name: &str, bytes: &[u8]) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some("json") if std::str::from_utf8(bytes).is_ok() => "application/json",
        Some("png") if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => "image/png",
        Some("jpg" | "jpeg") if bytes.starts_with(b"\xff\xd8") => "image/jpeg",
        _ if std::str::from_utf8(bytes).is_ok() => "text/plain",
        _ => "application/octet-stream",
    }
}

fn retained_bytes(tx: &Transaction<'_>, attempt: i64) -> Result<i64> {
    Ok(tx.query_row("SELECT (SELECT coalesce(sum(size_bytes),0) FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL) + (SELECT coalesce(sum(size_bytes),0) FROM artifact_reservations WHERE attempt_id=?1)", [attempt], |r| r.get(0))?)
}

fn require_budget(tx: &Transaction<'_>, attempt: i64, name: &str, size: u64) -> Result<()> {
    require(
        size <= FILE_LIMIT,
        &format!("Artifact {name} exceeds the 8 MiB per-artifact limit"),
    )?;
    let used = retained_bytes(tx, attempt)?;
    require(
        used.saturating_add(size as i64) <= ATTEMPT_LIMIT as i64,
        &format!(
            "Artifact {name} exceeds the 64 MiB per-attempt artifact limit ({used} bytes already saved)"
        ),
    )
}

fn final_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let parts: Vec<_> = Path::new(relative).components().collect();
    require(
        parts.len() == 3 || parts.len() == 4,
        "Unsafe artifact destination",
    )?;
    require(
        parts
            .iter()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "Unsafe artifact destination",
    )?;
    let parent = root.join(relative).parent().unwrap().to_path_buf();
    fs::create_dir_all(&parent)?;
    require(
        parent.canonicalize()?.starts_with(root.canonicalize()?),
        "Artifact destination escapes state storage",
    )?;
    Ok(root.join(relative))
}

fn staged_path(final_path: &Path) -> PathBuf {
    final_path.with_file_name(format!(
        "{}.pending",
        final_path.file_name().unwrap().to_string_lossy()
    ))
}

fn publication_lock(root: &Path) -> Result<fs::File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(root.join("artifact-publication.lock"))?)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn verify(path: &Path, size: i64, checksum: &str) -> Result<bool> {
    let input = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.len() != size as u64 {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    input.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    Ok(bytes.len() as i64 == size && format!("{:x}", Sha256::digest(&bytes)) == checksum)
}

impl Service {
    pub(crate) fn publish_artifact(
        &self,
        attempt: i64,
        token: &str,
        name: &str,
        source: &Path,
        relative: &str,
        tree: &str,
    ) -> Result<()> {
        require(!name.trim().is_empty(), "Artifact name is required")?;
        require(
            matches!(tree.len(), 40 | 64),
            "Artifact source tree is required",
        )?;
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(source)?;
        let metadata = input.metadata()?;
        require(
            metadata.is_file(),
            &format!(
                "Artifact {name}: only regular files directly in KTASK_ARTIFACT_DIR are supported"
            ),
        )?;
        require(
            metadata.len() <= FILE_LIMIT,
            &format!("Artifact {name} exceeds the 8 MiB per-artifact limit"),
        )?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        input.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
        require(
            bytes.len() as u64 == metadata.len(),
            &format!("Artifact {name} changed while being archived"),
        )?;
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        let media = media_type(name, &bytes);
        let root = self.db.state_file("")?;
        let publication = publication_lock(&root)?;
        publication.lock()?;
        let path = final_path(&root, relative)?;
        let pending = staged_path(&path);
        require(
            !path.exists() && !pending.exists(),
            "Artifact destination already exists",
        )?;
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND project_id=?2 AND execution_token=?3)", params![attempt,self.project_id,token], |r| r.get::<_, bool>(0))?, "Runner lost ownership of artifact attempt")?;
        require_budget(&tx, attempt, name, bytes.len() as u64)?;
        tx.execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![attempt,name,media,relative,bytes.len() as i64,checksum,tree,now_ms()])?;
        tx.commit()?;
        // A crash from here on leaves a reservation that recovery can inspect.
        let result = (|| -> Result<()> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&pending)?;
            output.write_all(&bytes)?;
            output.sync_all()?;
            output.set_permissions(fs::Permissions::from_mode(0o444))?;
            fs::rename(&pending, &path)?;
            FileSync::parent(&path)?;
            self.finish_reservation(relative)?;
            Ok(())
        })();
        if result.is_err()
            && !verify(&path, bytes.len() as i64, &checksum).unwrap_or(false)
            && !verify(&pending, bytes.len() as i64, &checksum).unwrap_or(false)
            && remove_if_present(&pending).is_ok()
            && remove_if_present(&path).is_ok()
        {
            let _ = self.db.connection().execute(
                "DELETE FROM artifact_reservations WHERE relative_path=?1",
                [relative],
            );
        }
        result
    }

    fn finish_reservation(&self, relative: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) SELECT attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms FROM artifact_reservations WHERE relative_path=?1 ON CONFLICT(relative_path) DO NOTHING", [relative])?;
        tx.execute(
            "DELETE FROM artifact_reservations WHERE relative_path=?1",
            [relative],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reconcile_artifact_reservations(&self) -> Result<()> {
        let pending: bool = self.db.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM artifact_reservations r JOIN attempts a ON a.id=r.attempt_id WHERE a.project_id=?1)",
            [self.project_id], |r| r.get(0))?;
        if !pending {
            return Ok(());
        }
        let root = self.db.state_file("")?;
        // A reader can open while either a guardian or the parent supervisor is
        // publishing a transcript. Never reconcile their live reservation.
        let publication = publication_lock(&root)?;
        match publication.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut stmt = self.db.connection().prepare("SELECT relative_path,size_bytes,checksum_sha256 FROM artifact_reservations JOIN attempts ON attempts.id=attempt_id WHERE attempts.project_id=?1 ORDER BY artifact_reservations.id")?;
        let rows = stmt
            .query_map([self.project_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (relative, size, checksum) in rows {
            let path = final_path(&root, &relative)?;
            let pending = staged_path(&path);
            if verify(&path, size, &checksum)? {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
                self.finish_reservation(&relative)?;
            } else if verify(&pending, size, &checksum)? {
                fs::set_permissions(&pending, fs::Permissions::from_mode(0o444))?;
                fs::rename(&pending, &path)?;
                FileSync::parent(&path)?;
                self.finish_reservation(&relative)?;
            } else {
                // The process stopped before a complete copy; no evidence was published.
                remove_if_present(&pending)?;
                remove_if_present(&path)?;
                let tx = Transaction::new_unchecked(
                    self.db.connection(),
                    TransactionBehavior::Immediate,
                )?;
                tx.execute(
                    "DELETE FROM artifact_reservations WHERE relative_path=?1",
                    [&relative],
                )?;
                tx.commit()?;
            }
        }
        Ok(())
    }
}

struct FileSync;
impl FileSync {
    fn parent(path: &Path) -> Result<()> {
        fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_limits_report_file_and_attempt_overflow() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let project = scratch.path().join("project");
        fs::create_dir(&project)?;
        let state = scratch.path().join("state");
        let mut service =
            Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
        let models = ["sonnet", "opus", "haiku", "opus"].map(|model| {
            Some(ModelConfig {
                provider: "claude".into(),
                model: model.into(),
                effort: None,
            })
        });
        let task = service.create(
            &[Definition {
                title: "Artifact limits".into(),
                goal: "Keep evidence within the attempt budget".into(),
                kind: Kind::Ai,
                criteria: vec!["Retain accepted files".into()],
                models,
                attempt_limit: Some(1),
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        let target = VersionedTask {
            id: task,
            version: service.current_task()?.expect("current task").version,
        };
        let run = service.begin_run(target, "artifact-limit-token", "fixture-base", false)?;
        let folder = state.join(format!("evidence/attempt-{}/step-1", run.attempt));
        let relative = |name: &str| format!("evidence/attempt-{}/step-1/{name}", run.attempt);
        let source = scratch.path().join("source");
        let tree = "a".repeat(40);

        fs::write(&source, vec![b'x'; FILE_LIMIT as usize + 1])?;
        assert!(
            service
                .publish_artifact(
                    run.attempt,
                    &run.token,
                    "oversized.txt",
                    &source,
                    &relative("oversized"),
                    &tree,
                )
                .unwrap_err()
                .to_string()
                .contains("8 MiB per-artifact")
        );
        assert!(!folder.join("oversized").exists());

        fs::write(&source, vec![b'x'; FILE_LIMIT as usize])?;
        for index in 0..8 {
            service.publish_artifact(
                run.attempt,
                &run.token,
                "capture.txt",
                &source,
                &relative(&format!("file-0-{index}")),
                &tree,
            )?;
        }
        fs::write(&source, b"x")?;
        assert!(
            service
                .publish_artifact(
                    run.attempt,
                    &run.token,
                    "extra.txt",
                    &source,
                    &relative("file-0-8"),
                    &tree,
                )
                .unwrap_err()
                .to_string()
                .contains("64 MiB per-attempt")
        );

        let (count, recorded): (i64, i64) = service.db.connection().query_row(
            "SELECT count(*),sum(size_bytes) FROM artifacts WHERE attempt_id=?1",
            [run.attempt],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(count, 8);
        assert_eq!(recorded, ATTEMPT_LIMIT as i64);
        let retained = fs::read_dir(&folder)?
            .map(|entry| {
                entry
                    .and_then(|entry| entry.metadata())
                    .map(|metadata| metadata.len())
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(retained.len(), 8);
        assert_eq!(retained.into_iter().sum::<u64>(), ATTEMPT_LIMIT);
        assert!(!folder.join("file-0-8").exists());
        assert!(!folder.join("file-0-8.pending").exists());
        let reservations: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM artifact_reservations WHERE attempt_id=?1",
            [run.attempt],
            |row| row.get(0),
        )?;
        assert_eq!(reservations, 0);
        Ok(())
    }
}

//! Atomic admission of deliberate evidence. A reservation records the budget and
//! provenance before bytes enter state storage; opening the project reconciles an
//! interrupted copy before exposing it to readers or retention.
use super::*;
use nix::{
    errno::Errno,
    fcntl::{OFlag, RenameFlags, openat, renameat2},
    sys::stat::{Mode, mkdirat},
    unistd::{UnlinkatFlags, unlinkat},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions, TryLockError},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

const FILE_LIMIT: u64 = 128 * 1024 * 1024;
const ATTEMPT_LIMIT: u64 = 1024 * 1024 * 1024;
const COUNT_LIMIT: u64 = 4096;

fn budget_label(bytes: u64) -> String {
    if bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{bytes} bytes")
    }
}

fn media_type(name: &str, bytes: &[u8], text: bool) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some("json") if text => "application/json",
        Some("png") if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => "image/png",
        Some("jpg" | "jpeg") if bytes.starts_with(b"\xff\xd8") => "image/jpeg",
        _ if text => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Preserve UTF-8 validation across streamed chunk boundaries.
fn valid_utf8_chunk(tail: &mut Vec<u8>, bytes: &[u8]) -> bool {
    tail.extend_from_slice(bytes);
    match std::str::from_utf8(tail) {
        Ok(_) => {
            tail.clear();
            true
        }
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            tail.drain(..valid);
            true
        }
        Err(_) => {
            tail.clear();
            false
        }
    }
}

fn retained_bytes(tx: &Transaction<'_>, attempt: i64) -> Result<i64> {
    Ok(tx.query_row("SELECT (SELECT coalesce(sum(size_bytes),0) FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL) + (SELECT coalesce(sum(size_bytes),0) FROM artifact_reservations WHERE attempt_id=?1)", [attempt], |r| r.get(0))?)
}

fn require_budget(tx: &Transaction<'_>, attempt: i64, name: &str, size: u64) -> Result<()> {
    let file_limit = crate::resource::limit("KTASK_ARTIFACT_FILE_BYTES", FILE_LIMIT);
    let attempt_limit = crate::resource::limit("KTASK_ARTIFACT_ATTEMPT_BYTES", ATTEMPT_LIMIT);
    let count_limit = crate::resource::limit("KTASK_ARTIFACT_ATTEMPT_FILES", COUNT_LIMIT);
    require(
        size <= file_limit,
        &format!(
            "Artifact {name} exceeds the {} per-artifact limit",
            budget_label(file_limit)
        ),
    )?;
    let used = retained_bytes(tx, attempt)?;
    require(
        used.saturating_add(size as i64) <= attempt_limit as i64,
        &format!(
            "Artifact {name} exceeds the {} per-attempt artifact limit ({used} bytes already saved)",
            budget_label(attempt_limit)
        ),
    )?;
    let count: i64 = tx.query_row("SELECT (SELECT count(*) FROM artifacts WHERE attempt_id=?1 AND pruned_at_ms IS NULL) + (SELECT count(*) FROM artifact_reservations WHERE attempt_id=?1)", [attempt], |r| r.get(0))?;
    require(
        count < count_limit as i64,
        &format!(
            "Artifact count limit reached ({count_limit} per attempt, including pending reservations)"
        ),
    )
}

struct ArtifactTarget {
    parent: fs::File,
    name: String,
}

struct Publication<'a> {
    attempt: i64,
    token: &'a str,
    name: &'a str,
    source: &'a Path,
    relative: &'a str,
    tree: &'a str,
}

impl ArtifactTarget {
    fn pending(&self) -> String {
        format!("{}.pending", self.name)
    }

    fn open(&self, name: &str) -> Result<Option<fs::File>> {
        match openat(
            &self.parent,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Some(fs::File::from(fd))),
            Err(Errno::ENOENT) => Ok(None),
            Err(error) => Err(io(error)),
        }
    }

    fn absent(&self) -> Result<bool> {
        Ok(self.open(&self.name)?.is_none() && self.open(&self.pending())?.is_none())
    }

    fn remove(&self, name: &str) -> Result<()> {
        match unlinkat(&self.parent, name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(error) => Err(io(error)),
        }
    }

    fn publish_pending(&self) -> Result<()> {
        renameat2(
            &self.parent,
            self.pending().as_str(),
            &self.parent,
            self.name.as_str(),
            RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io)?;
        self.parent.sync_all()?;
        Ok(())
    }
}

/// Resolve each component from a pinned directory, rejecting substituted links.
/// Recovery never creates a parent; a missing one means the copy never completed.
fn artifact_target(
    root: &fs::File,
    relative: &str,
    create_parent: bool,
) -> Result<Option<ArtifactTarget>> {
    let path = super::evidence_path::EvidencePath::parse(relative)
        .ok_or_else(|| Error::Conflict("Unsafe artifact destination".into()))?;
    let mut parent = root.try_clone()?;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    for name in path.parents() {
        let next = match openat(&parent, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) if create_parent => {
                match mkdirat(&parent, name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(error) => return Err(io(error)),
                }
                openat(&parent, name, flags, Mode::empty()).map_err(io)?
            }
            Err(Errno::ENOENT) => return Ok(None),
            Err(error) => return Err(io(error)),
        };
        parent = fs::File::from(next);
    }
    Ok(Some(ArtifactTarget {
        parent,
        name: path.name.to_owned(),
    }))
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

fn verify(target: &ArtifactTarget, name: &str, size: i64, checksum: &str) -> Result<bool> {
    let input = match target.open(name)? {
        Some(input) => input,
        None => return Ok(false),
    };
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.len() != size as u64 {
        return Ok(false);
    }
    let mut input = input;
    let mut hash = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        read = read.saturating_add(n as u64);
        if read > size as u64 {
            return Ok(false);
        }
        hash.update(&buffer[..n]);
    }
    Ok(read == size as u64 && format!("{:x}", hash.finalize()) == checksum)
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
        self.publish_artifact_after_reservation(
            Publication {
                attempt,
                token,
                name,
                source,
                relative,
                tree,
            },
            || {},
        )
    }

    fn publish_artifact_after_reservation(
        &self,
        publication: Publication<'_>,
        after_reservation: impl FnOnce(),
    ) -> Result<()> {
        let Publication {
            attempt,
            token,
            name,
            source,
            relative,
            tree,
        } = publication;
        require(!name.trim().is_empty(), "Artifact name is required")?;
        require(
            matches!(tree.len(), 40 | 64),
            "Artifact source tree is required",
        )?;
        let mut input = OpenOptions::new()
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
        let file_limit = crate::resource::limit("KTASK_ARTIFACT_FILE_BYTES", FILE_LIMIT);
        require(
            metadata.len() <= file_limit,
            &format!(
                "Artifact {name} exceeds the {} per-artifact limit",
                budget_label(file_limit)
            ),
        )?;
        let size = metadata.len();
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut prefix = Vec::new();
        let mut read = 0u64;
        let mut utf8_tail = Vec::new();
        let mut text = true;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            read = read.saturating_add(n as u64);
            require(read <= size, "Artifact changed while being archived")?;
            if prefix.is_empty() {
                prefix.extend_from_slice(&buffer[..n.min(512)]);
            }
            if text {
                text = valid_utf8_chunk(&mut utf8_tail, &buffer[..n]);
            }
            hash.update(&buffer[..n]);
        }
        require(read == size, "Artifact changed while being archived")?;
        let checksum = format!("{:x}", hash.finalize());
        let media = media_type(name, &prefix, text && utf8_tail.is_empty());
        input.seek(SeekFrom::Start(0))?;
        let root = self.db.state_dir();
        let publication = publication_lock(&self.db.state_file("")?)?;
        publication.lock()?;
        let existing = artifact_target(root, relative, false)?;
        if let Some(target) = existing.as_ref() {
            require(target.absent()?, "Artifact destination already exists")?;
        }
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND project_id=?2 AND execution_token=?3)", params![attempt,self.project_id,token], |r| r.get::<_, bool>(0))?, "Runner lost ownership of artifact attempt")?;
        require_budget(&tx, attempt, name, size)?;
        super::retention::admit_capacity(
            &tx,
            super::retention::measure_known_bytes(&self.db)?,
            0,
            0,
            size,
            1,
        )?;
        tx.execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![attempt,name,media,relative,size as i64,checksum,tree,now_ms()])?;
        tx.commit()?;
        // A crash from here on leaves a reservation that recovery can inspect.
        after_reservation();
        let mut target = None;
        let mut staged_created = false;
        let mut published = false;
        let result = (|| -> Result<()> {
            let safe = artifact_target(root, relative, true)?.expect("parent was created");
            require(safe.absent()?, "Artifact destination already exists")?;
            let pending = safe.pending();
            target = Some(safe);
            let safe = target.as_ref().unwrap();
            let mut output = fs::File::from(
                openat(
                    &safe.parent,
                    pending.as_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    Mode::from_bits_truncate(0o600),
                )
                .map_err(io)?,
            );
            staged_created = true;
            let mut copied = 0u64;
            let mut copy_hash = Sha256::new();
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                copied = copied.saturating_add(n as u64);
                require(copied <= size, "Artifact changed while being archived")?;
                copy_hash.update(&buffer[..n]);
                output.write_all(&buffer[..n])?;
            }
            require(
                copied == size && format!("{:x}", copy_hash.finalize()) == checksum,
                "Artifact changed while being archived",
            )?;
            output.sync_all()?;
            output.set_permissions(fs::Permissions::from_mode(0o444))?;
            safe.publish_pending()?;
            published = true;
            self.finish_reservation(relative)?;
            Ok(())
        })();
        if result.is_err()
            && let Some(safe) = target.as_ref()
            && staged_created
            && !published
            && safe.remove(&safe.pending()).is_ok()
        {
            let _ = self.drop_reservation(relative);
        }
        result
    }

    pub(super) fn finish_reservation(&self, relative: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) SELECT attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms FROM artifact_reservations WHERE relative_path=?1 ON CONFLICT(relative_path) DO NOTHING", [relative])?;
        tx.execute(
            "DELETE FROM artifact_reservations WHERE relative_path=?1",
            [relative],
        )?;
        retention::bump_admission_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn drop_reservation(&self, relative: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM artifact_reservations WHERE relative_path=?1",
            [relative],
        )?;
        retention::bump_admission_generation(&tx)?;
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
        let root = self.db.state_dir();
        // A reader can open while either a guardian or the parent supervisor is
        // publishing a transcript. Never reconcile their live reservation.
        let publication = publication_lock(&self.db.state_file("")?)?;
        match publication.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut cursor = 0_i64;
        loop {
            let mut stmt = self.db.connection().prepare("SELECT artifact_reservations.id,relative_path,size_bytes,checksum_sha256 FROM artifact_reservations JOIN attempts ON attempts.id=attempt_id WHERE attempts.project_id=?1 AND artifact_reservations.id>?2 ORDER BY artifact_reservations.id LIMIT 128")?;
            let rows = stmt
                .query_map(params![self.project_id, cursor], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if rows.is_empty() {
                break;
            }
            for (id, relative, size, checksum) in rows {
                cursor = id;
                let Some(target) = artifact_target(root, &relative, false)? else {
                    self.drop_reservation(&relative)?;
                    continue;
                };
                if verify(&target, &target.name, size, &checksum)? {
                    target
                        .open(&target.name)?
                        .unwrap()
                        .set_permissions(fs::Permissions::from_mode(0o444))?;
                    self.finish_reservation(&relative)?;
                } else if verify(&target, &target.pending(), size, &checksum)? {
                    target
                        .open(&target.pending())?
                        .unwrap()
                        .set_permissions(fs::Permissions::from_mode(0o444))?;
                    target.publish_pending()?;
                    self.finish_reservation(&relative)?;
                } else {
                    // The process stopped before a complete copy; no evidence was published.
                    target.remove(&target.pending())?;
                    target.remove(&target.name)?;
                    self.drop_reservation(&relative)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn streamed_utf8_handles_boundaries_and_late_invalid_bytes() {
        let mut tail = Vec::new();
        assert!(valid_utf8_chunk(&mut tail, &[b'x', 0xe2]));
        assert!(valid_utf8_chunk(&mut tail, &[0x9c, 0x93]));
        assert!(tail.is_empty());
        assert!(!valid_utf8_chunk(&mut tail, &[0xff]));
        assert_eq!(
            media_type("binary.log", b"valid prefix", false),
            "application/octet-stream"
        );
    }

    #[test]
    fn substituted_recovery_parent_never_touches_external_evidence() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let project = scratch.path().join("project");
        let other_state = scratch.path().join("other-instance");
        fs::create_dir(&project)?;
        let other = crate::storage::Database::open_in(&other_state)?;
        let other_identity = other
            .connection()
            .query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))?;
        let state = scratch.path().join("state");
        let mut service =
            Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
        let task = service.create(
            &[Definition {
                title: "Recover proof".into(),
                goal: "Keep outside files unchanged".into(),
                kind: Kind::Ai,
                criteria: vec!["Proof".into()],
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
            }],
            None,
            true,
        )?[0];
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            "substituted-parent",
            "fixture-base",
            false,
        )?;
        let own_parent = state.join(format!("evidence/attempt-{}/step-1", run.attempt));
        fs::create_dir_all(own_parent.parent().unwrap())?;
        let external_project = project.join("external-proof");
        fs::create_dir(&external_project)?;
        let source = scratch.path().join("source");
        fs::write(&source, b"proof")?;
        let checksum = format!("{:x}", Sha256::digest(b"proof"));
        let other_db = fs::metadata(other_state.join("ktask.sqlite3"))?;
        for (index, parent, valid_final, valid_pending) in [
            (0, &external_project, true, false),
            (1, &other_state, false, true),
            (2, &external_project, false, false),
            (3, &other_state, false, false),
        ] {
            let name = format!("file-0-{index}");
            let final_file = parent.join(&name);
            let pending_file = parent.join(format!("{name}.pending"));
            fs::write(
                &final_file,
                if valid_final {
                    b"proof".as_slice()
                } else {
                    b"outside-final"
                },
            )?;
            fs::write(
                &pending_file,
                if valid_pending {
                    b"proof".as_slice()
                } else {
                    b"outside-pending"
                },
            )?;
            fs::set_permissions(&final_file, fs::Permissions::from_mode(0o600))?;
            fs::set_permissions(&pending_file, fs::Permissions::from_mode(0o640))?;
            let final_before = fs::read(&final_file)?;
            let pending_before = fs::read(&pending_file)?;
            symlink(parent, &own_parent)?;
            let relative = format!("evidence/attempt-{}/step-1/{name}", run.attempt);
            service.db.connection().execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,'proof','text/plain',?2,5,?3,?4,1)", params![run.attempt,relative,checksum,"a".repeat(40)])?;
            assert!(service.reconcile_artifact_reservations().is_err());
            assert!(
                service
                    .publish_artifact(
                        run.attempt,
                        &run.token,
                        "proof",
                        &source,
                        &relative,
                        &"a".repeat(40)
                    )
                    .is_err()
            );
            assert_eq!(fs::read(&final_file)?, final_before);
            assert_eq!(fs::read(&pending_file)?, pending_before);
            assert_eq!(
                fs::metadata(&final_file)?.permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&pending_file)?.permissions().mode() & 0o777,
                0o640
            );
            let pending: i64 = service.db.connection().query_row(
                "SELECT count(*) FROM artifact_reservations WHERE relative_path=?1",
                [&relative],
                |r| r.get(0),
            )?;
            assert_eq!(pending, 1);
            service.drop_reservation(&relative)?;
            fs::remove_file(&own_parent)?;
        }
        let raced_parent = state.join(format!("evidence/attempt-{}/step-2", run.attempt));
        let raced_relative = format!("evidence/attempt-{}/step-2/file-0-9", run.attempt);
        let outside_final = external_project.join("file-0-9");
        let outside_pending = external_project.join("file-0-9.pending");
        fs::write(&outside_final, b"outside-final")?;
        fs::write(&outside_pending, b"outside-pending")?;
        assert!(
            service
                .publish_artifact_after_reservation(
                    Publication {
                        attempt: run.attempt,
                        token: &run.token,
                        name: "proof",
                        source: &source,
                        relative: &raced_relative,
                        tree: &"a".repeat(40)
                    },
                    || symlink(&external_project, &raced_parent).unwrap(),
                )
                .is_err()
        );
        assert_eq!(fs::read(&outside_final)?, b"outside-final");
        assert_eq!(fs::read(&outside_pending)?, b"outside-pending");
        let pending: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM artifact_reservations WHERE relative_path=?1",
            [&raced_relative],
            |r| r.get(0),
        )?;
        assert_eq!(
            pending, 1,
            "failed validation must retain recovery ownership"
        );
        assert_eq!(
            other
                .connection()
                .query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))?,
            other_identity
        );
        let other_after = fs::metadata(other_state.join("ktask.sqlite3"))?;
        assert_eq!(
            (other_db.dev(), other_db.ino()),
            (other_after.dev(), other_after.ino())
        );
        Ok(())
    }

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

        // A real 100 MiB file is copied and hashed with bounded buffers, not loaded into a prompt.
        let mut out = fs::File::create(&source)?;
        out.write_all(b"failure near the start\n")?;
        out.set_len(100 * 1024 * 1024)?;
        service.publish_artifact(
            run.attempt,
            &run.token,
            "large.log",
            &source,
            &relative("file-0-0"),
            &tree,
        )?;
        fs::write(&source, b"small capture fixture")?;
        for index in 1..95 {
            let source_tree = if index % 3 == 0 {
                tree.clone()
            } else if index % 3 == 1 {
                "b".repeat(40)
            } else {
                "c".repeat(40)
            };
            service.publish_artifact(
                run.attempt,
                &run.token,
                "repeated.png",
                &source,
                &relative(&format!("file-0-{index}")),
                &source_tree,
            )?;
        }
        let packet = scratch.path().join("packet");
        let summary = service.review_bundle(
            run.attempt,
            &state,
            &packet,
            &tree,
            "Review scope: fixture diff",
        )?;
        assert!(summary.len() < 24 * 1024);
        assert!(!summary.contains("failure near the start"));
        assert!(summary.contains("95 artifacts"));
        let index = fs::read_to_string(packet.join("artifacts.jsonl"))?;
        assert_eq!(index.lines().count(), 95);
        assert!(index.contains("\"matches_current_tree\":false"));
        assert!(index.contains("\"matches_current_tree\":true"));
        assert_eq!(
            fs::metadata(folder.join("file-0-0"))?.len(),
            100 * 1024 * 1024
        );
        let reopened = Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
        let summary2 = reopened.review_bundle(
            run.attempt,
            &state,
            &packet,
            &tree,
            "Review scope: fixture diff",
        )?;
        assert_eq!(summary, summary2);
        // Finite storage budgets still reject oversized evidence before allocating memory.
        fs::File::create(&source)?.set_len(FILE_LIMIT + 1)?;
        assert!(
            reopened
                .publish_artifact(
                    run.attempt,
                    &run.token,
                    "too-big",
                    &source,
                    &relative("oversized"),
                    &tree
                )
                .is_err()
        );
        assert!(!folder.join("oversized").exists());
        let tx =
            Transaction::new_unchecked(reopened.db.connection(), TransactionBehavior::Immediate)?;
        assert!(require_budget(&tx, run.attempt, "too-much", ATTEMPT_LIMIT).is_err());

        Ok(())
    }
}

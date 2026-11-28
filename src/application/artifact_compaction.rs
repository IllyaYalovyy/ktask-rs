//! Compact the archive in bounded pages without changing any reference's path.
use super::*;
use sha2_checkpoint::{
    Digest as CheckpointDigest, Sha256 as CheckpointHash,
    digest::common::hazmat::{SerializableState, SerializedState},
};

const PAGE: usize = 16;
const PAGE_BYTES: u64 = 16 * 1024 * 1024;
const HASH_BLOCK: u64 = 64;

impl Service {
    pub(in crate::application) fn compact_artifacts(&self) -> Result<CompactionProgress> {
        Ok(self.compact_artifact_page(PAGE_BYTES)?.0)
    }

    /// Returns the page's progress and how many payload bytes it read.
    pub(in crate::application) fn compact_artifact_page(
        &self,
        limit: u64,
    ) -> Result<(CompactionProgress, u64)> {
        use CompactionProgress::{Done, More, PublicationBusy};

        let db = self.db.connection();
        let pending: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM artifacts WHERE needs_compaction=1)",
            [],
            |row| row.get(0),
        )?;
        if !pending {
            return Ok((Done, 0));
        }
        // Select the page under the lock. Another compactor may have processed it
        // since the probe above.
        let lock = publication_lock(&self.db.state_file("")?)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok((PublicationBusy, 0)),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let rows = db
            .prepare(
                "SELECT id,attempt_id,relative_path,size_bytes,checksum_sha256 FROM artifacts
                 WHERE needs_compaction=1 ORDER BY id LIMIT ?1",
            )?
            .query_map([PAGE as i64], |row| {
                Ok((
                    ArchiveFile {
                        id: row.get(0)?,
                        attempt: row.get(1)?,
                        relative: row.get(2)?,
                    },
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut remaining = limit;
        for (entry, size, checksum) in &rows {
            if remaining == 0
                || !self.compact_artifact(entry, *size, checksum.as_deref(), &mut remaining)?
            {
                return Ok((More, limit - remaining));
            }
            let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE artifacts SET needs_compaction=0 WHERE id=?1",
                [entry.id],
            )?;
            tx.execute("DELETE FROM artifact_compaction_hashes", [])?;
            tx.commit()?;
        }
        let progress = if rows.len() == PAGE { More } else { Done };
        Ok((progress, limit - remaining))
    }

    /// False means verification was checkpointed and must continue next page.
    fn compact_artifact(
        &self,
        entry: &ArchiveFile,
        size: i64,
        checksum: Option<&str>,
        remaining: &mut u64,
    ) -> Result<bool> {
        let db = self.db.connection();
        let root = self.db.state_dir();
        if !retained(db, entry.id)? {
            return Ok(true);
        }
        let Some(target) = generated_target(root, entry.attempt, &entry.relative)? else {
            return Ok(true);
        };
        let pending = format!("{}.dedup", target.name);
        // A stopped pass may have linked the source or already renamed it. The
        // published path is intact in either case.
        target.remove(&pending)?;
        let Some(mut current) = CompactionFile::open(target, size, false)? else {
            return Ok(true);
        };
        let actual = match current.hash_page(db, entry.id, remaining)? {
            HashPage::Pending => return Ok(false),
            HashPage::Changed => return Ok(false),
            HashPage::Complete(actual) if checksum.is_none_or(|expected| expected == actual) => {
                actual
            }
            HashPage::Complete(_) => return Ok(true),
        };

        let shared = match self.compaction_source(entry.id, &current, &actual, remaining)? {
            SourceSearch::Pending => return Ok(false),
            SourceSearch::Found(id, source) => Some((id, source)),
            SourceSearch::Exhausted => None,
            SourceSearch::AlreadyShared => {
                // This also settles accounting if a previous process stopped
                // after rename but before updating the allocation generation.
                current.target.parent.sync_all()?;
                retention::evidence_allocation_changed(db)?;
                None
            }
        };
        if !current.still_current()? || !retained(db, entry.id)? {
            return Ok(false);
        }
        if let Some((id, source)) = shared {
            if !source.still_current()? || !retained(db, id)? {
                return Ok(false);
            }
            retention::bump_admission_generation(db)?;
            current.target.link(&source.file, &pending)?;
            nix::fcntl::renameat(
                &current.target.parent,
                pending.as_str(),
                &current.target.parent,
                current.target.name.as_str(),
            )
            .map_err(io)?;
            current.target.parent.sync_all()?;
            retention::evidence_allocation_changed(db)?;
        }
        if checksum.is_none() {
            db.execute(
                "UPDATE artifacts SET checksum_sha256=?2 WHERE id=?1 AND checksum_sha256 IS NULL",
                params![entry.id, actual],
            )?;
        }
        Ok(true)
    }

    fn compaction_source(
        &self,
        id: i64,
        current: &CompactionFile,
        checksum: &str,
        remaining: &mut u64,
    ) -> Result<SourceSearch> {
        let db = self.db.connection();
        let mut after = db.query_row(
            "SELECT source_after_id FROM artifact_compaction_hashes WHERE artifact_id=?1",
            [id],
            |row| row.get(0),
        )?;
        let candidates =
            shared_candidates(db, checksum, current.size as i64, true, (false, after))?;
        let more = candidates.len() == SHARED_CANDIDATE_PAGE;
        for (candidate, _) in candidates {
            if let Some(target) =
                generated_target(self.db.state_dir(), candidate.attempt, &candidate.relative)?
                && let Some(mut source) = CompactionFile::open(target, current.size as i64, true)?
            {
                if source.inode == current.inode {
                    return Ok(SourceSearch::AlreadyShared);
                }
                match source.hash_page(db, candidate.id, remaining) {
                    Ok(HashPage::Pending | HashPage::Changed) => return Ok(SourceSearch::Pending),
                    Ok(HashPage::Complete(hash)) if hash == checksum => {
                        return Ok(SourceSearch::Found(candidate.id, source));
                    }
                    Ok(HashPage::Complete(_)) | Err(Error::Io(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            after = candidate.id;
            let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
            tx.execute(
                "DELETE FROM artifact_compaction_hashes WHERE artifact_id=?1",
                [after],
            )?;
            tx.execute(
                "UPDATE artifact_compaction_hashes SET source_after_id=?2 WHERE artifact_id=?1",
                params![id, after],
            )?;
            tx.commit()?;
        }
        Ok(if more {
            SourceSearch::Pending
        } else {
            SourceSearch::Exhausted
        })
    }
}

enum SourceSearch {
    Pending,
    Found(i64, CompactionFile),
    AlreadyShared,
    Exhausted,
}

fn retained(db: &rusqlite::Connection, id: i64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=?1 AND pruned_at_ms IS NULL AND pruning_requested_at_ms IS NULL)",
        [id],
        |row| row.get(0),
    )?)
}

struct CompactionFile {
    target: ArtifactTarget,
    file: fs::File,
    identity: String,
    inode: (u64, u64),
    size: u64,
}

enum HashPage {
    Pending,
    Changed,
    Complete(String),
}

impl CompactionFile {
    fn open(target: ArtifactTarget, size: i64, source: bool) -> Result<Option<Self>> {
        let Ok(Some(file)) = target.open(&target.name) else {
            return Ok(None);
        };
        let mut metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != size as u64 {
            return Ok(None);
        }
        if metadata.mode() & 0o222 != 0 {
            if source || metadata.nlink() > 1 {
                return Ok(None);
            }
            file.set_permissions(fs::Permissions::from_mode(0o444))?;
            file.sync_all()?;
            metadata = file.metadata()?;
        }
        Ok(Some(Self {
            target,
            file,
            identity: identity(&metadata),
            inode: (metadata.dev(), metadata.ino()),
            size: size as u64,
        }))
    }

    fn still_current(&self) -> Result<bool> {
        let Ok(Some(file)) = self.target.open(&self.target.name) else {
            return Ok(false);
        };
        Ok(identity(&file.metadata()?) == self.identity)
    }

    fn hash_page(
        &mut self,
        db: &rusqlite::Connection,
        id: i64,
        remaining: &mut u64,
    ) -> Result<HashPage> {
        let saved = db
            .query_row(
                "SELECT bytes_read,hash_state FROM artifact_compaction_hashes
                 WHERE artifact_id=?1 AND identity=?2",
                params![id, self.identity],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        if let Some((offset, state)) = &saved
            && *offset == self.size
            && let Ok(digest) = <[u8; 32]>::try_from(state.as_slice())
        {
            return Ok(HashPage::Complete(digest_hex(digest)));
        }
        let (mut offset, mut hash) = saved
            .and_then(|(offset, state)| {
                let state = SerializedState::<CheckpointHash>::try_from(state.as_slice()).ok()?;
                Some((offset, CheckpointHash::deserialize(&state).ok()?))
            })
            .filter(|(offset, _)| *offset <= self.size)
            .unwrap_or_default();
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buffer = [0; 64 * 1024];
        // Persist only complete blocks so the serialized hasher contains no
        // buffered evidence bytes. At EOF persist just the finished digest.
        let mut available = self.size - offset;
        if available > *remaining {
            available = *remaining / HASH_BLOCK * HASH_BLOCK;
        }
        while available > 0 {
            let count = available.min(buffer.len() as u64) as usize;
            let mut chunk = (&mut self.file).take(count as u64);
            let read = chunk.read_exact(&mut buffer[..count]);
            *remaining -= count as u64 - chunk.limit();
            read?;
            CheckpointDigest::update(&mut hash, &buffer[..count]);
            offset += count as u64;
            available -= count as u64;
        }
        if !self.still_current()? {
            return Ok(HashPage::Changed);
        }
        let digest: Option<[u8; 32]> =
            (offset == self.size).then(|| CheckpointDigest::finalize(hash.clone()).into());
        let state = hash.serialize();
        let saved = digest
            .as_ref()
            .map_or(state.as_slice(), |digest| digest.as_slice());
        db.execute(
            "INSERT INTO artifact_compaction_hashes(artifact_id,identity,bytes_read,hash_state)
             VALUES (?1,?2,?3,?4) ON CONFLICT(artifact_id) DO UPDATE
             SET identity=excluded.identity,bytes_read=excluded.bytes_read,hash_state=excluded.hash_state,
                 source_after_id=CASE WHEN identity=excluded.identity THEN source_after_id ELSE 0 END",
            params![id, self.identity, offset as i64, saved],
        )?;
        Ok(digest.map_or(HashPage::Pending, |digest| {
            HashPage::Complete(digest_hex(digest))
        }))
    }
}

fn digest_hex(digest: [u8; 32]) -> String {
    format!("{:x}", sha2::digest::Output::<Sha256>::from(digest))
}

fn identity(metadata: &fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

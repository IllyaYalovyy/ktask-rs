//! Archiving a worker's deliberate evidence after it has exited. The producer can no longer
//! fill the host, so this is housekeeping: it completes, or records what it left out, and
//! never stops the attempt. The scratch entry limit already bounds what there is to read.
use super::{artifacts::retained, resource_breach::effective_limit, *};
use crate::resource::Limit;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

/// Manifest rows written per transaction.
const BATCH: usize = 512;
/// Publication failures in a row after which the remaining files are only listed.
const GIVE_UP: u32 = 16;

/// What an attempt's truncated evidence holds and lacks.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Truncated {
    pub total_files: u64,
    pub total_bytes: u64,
    pub archived_files: u64,
    pub archived_bytes: u64,
    /// Why files were left out, one line for each truncated collection.
    pub note: String,
}

impl Truncated {
    pub fn label(&self) -> String {
        // Files that were gone before they could be listed have no counts.
        if self.total_files == 0 {
            return "Evidence truncated: its files could not be read".into();
        }
        format!(
            "Evidence truncated: {} of {} files archived ({} of {} bytes), {} not archived",
            self.archived_files,
            self.total_files,
            self.archived_bytes,
            self.total_bytes,
            self.total_files.saturating_sub(self.archived_files)
        )
    }

    /// The statement reviewers and validators are given.
    pub fn fact(&self) -> String {
        format!(
            "Supervisor fact: {}. {} The manifest lists every file with its size and checksum, archived or not; a file that was not archived is not proof.",
            self.label(),
            self.note
        )
    }
}

/// One file of a truncated collection.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ManifestEntry {
    pub name: String,
    pub size_bytes: u64,
    pub checksum_sha256: Option<String>,
    /// Why it was not archived; None when it was.
    pub omitted: Option<String>,
}

pub(super) fn truncated(db: &Connection, attempt: i64) -> Result<Option<Truncated>> {
    let mut statement = db.prepare_cached(
        "SELECT total_files,total_bytes,archived_files,archived_bytes,coalesce(note,'')
         FROM evidence_collections WHERE attempt_id=?1 AND state='truncated' ORDER BY id",
    )?;
    let rows = statement
        .query_map([attempt], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((!rows.is_empty()).then(|| {
        rows.into_iter().fold(
            Truncated {
                total_files: 0,
                total_bytes: 0,
                archived_files: 0,
                archived_bytes: 0,
                note: String::new(),
            },
            |mut sum, (files, bytes, archived, archived_bytes, note)| {
                sum.total_files += files;
                sum.total_bytes = sum.total_bytes.saturating_add(bytes);
                sum.archived_files += archived;
                sum.archived_bytes += archived_bytes;
                if !sum.note.is_empty() {
                    sum.note.push(' ');
                }
                sum.note.push_str(&note);
                sum
            },
        )
    }))
}

/// One worker's artifact directory, to be archived under `prefix`.
pub(crate) struct Collection<'a> {
    pub attempt: i64,
    pub token: &'a str,
    pub prefix: &'a str,
    pub source: &'a Path,
    pub tree: &'a str,
}

/// Deterministic archive order: transcripts, other text, images, anything else; then name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Transcript,
    Text,
    Image,
    Other,
}

struct Found {
    name: OsString,
    size: u64,
    rank: Rank,
    /// Why it can never be archived.
    unusable: Option<&'static str>,
}

fn open_regular(path: &Path) -> std::io::Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn rank(path: &Path, name: &str) -> Rank {
    let mut head = Vec::new();
    if open_regular(path)
        .and_then(|file| file.take(8192).read_to_end(&mut head))
        .is_err()
    {
        return Rank::Other;
    }
    let image = head.starts_with(b"\x89PNG\r\n\x1a\n")
        || head.starts_with(b"\xff\xd8")
        || head.starts_with(b"GIF8")
        || head.get(8..12) == Some(b"WEBP");
    if image {
        return Rank::Image;
    }
    let text = !head.contains(&0)
        && match std::str::from_utf8(&head) {
            Ok(_) => true,
            // A character cut at the end of the sample is still text.
            Err(error) => error.error_len().is_none(),
        };
    let name = name.to_ascii_lowercase();
    if !text {
        Rank::Other
    } else if name.ends_with(".log") || name.contains("transcript") || name.contains("check") {
        Rank::Transcript
    } else {
        Rank::Text
    }
}

/// Every entry of the flat artifact directory, in archive order.
fn list(source: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(entry.path())?;
        let unusable = if !metadata.is_file() {
            Some("only regular files directly in KTASK_ARTIFACT_DIR are supported")
        } else if name.to_str().is_none_or(|name| name.trim().is_empty()) {
            Some("name is blank or not UTF-8")
        } else {
            None
        };
        let rank = if unusable.is_some() {
            Rank::Other
        } else {
            rank(&entry.path(), &name.to_string_lossy())
        };
        found.push(Found {
            size: if metadata.is_file() {
                metadata.len()
            } else {
                0
            },
            name,
            rank,
            unusable,
        });
    }
    found.sort_by(|a, b| (a.rank, a.name.as_bytes()).cmp(&(b.rank, b.name.as_bytes())));
    Ok(found)
}

fn checksum(path: &Path) -> std::io::Result<String> {
    let mut input = open_regular(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..read]);
    }
}

/// Removes the retained directory. A worker may have locked it against its owner.
fn discard(source: &Path) -> std::io::Result<()> {
    crate::scratch::remove_tree(source)
}

impl Service {
    /// Every file of the attempt's truncated collections, archived or not.
    pub fn evidence_manifest(&self, attempt: i64) -> Result<Vec<ManifestEntry>> {
        let mut statement = self.db.connection().prepare(
            "SELECT m.name,m.size_bytes,m.checksum_sha256,m.omitted FROM evidence_manifest m
             JOIN evidence_collections c ON c.id=m.collection_id JOIN attempts a ON a.id=c.attempt_id
             WHERE c.attempt_id=?1 AND a.project_id=?2 ORDER BY c.id,m.ordinal",
        )?;
        let entries = statement
            .query_map(params![attempt, self.project_id], |r| {
                Ok(ManifestEntry {
                    name: r.get(0)?,
                    size_bytes: r.get::<_, i64>(1)? as u64,
                    checksum_sha256: r.get(2)?,
                    omitted: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries)
    }

    /// Record, before the files move or are read, where an unfinished collection keeps them.
    pub(crate) fn reserve_collection(&self, collection: &Collection<'_>) -> Result<i64> {
        let db = self.db.connection();
        let existing: Option<i64> = db
            .query_row(
                "SELECT id FROM evidence_collections WHERE attempt_id=?1 AND relative_prefix=?2 AND state='collecting'",
                params![collection.attempt, collection.prefix],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            db.execute(
                "UPDATE evidence_collections SET source_path=?2 WHERE id=?1",
                params![id, collection.source.as_os_str().as_bytes()],
            )?;
            return Ok(id);
        }
        db.execute(
            "INSERT INTO evidence_collections(attempt_id,relative_prefix,source_path,source_tree,state) VALUES (?1,?2,?3,?4,'collecting')",
            params![
                collection.attempt,
                collection.prefix,
                collection.source.as_os_str().as_bytes(),
                collection.tree
            ],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// Register a disk handoff and its visible note together. The same handoff can be
    /// replayed after a crash, including after its collection has already closed.
    pub(crate) fn import_deferred_collection(
        &self,
        collection: &Collection<'_>,
        note: &str,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        let task: Option<i64> = tx
            .query_row(
                "SELECT task_id FROM attempts WHERE id=?1 AND project_id=?2 AND execution_token=?3",
                params![collection.attempt, self.project_id, collection.token],
                |r| r.get(0),
            )
            .optional()?;
        let Some(task) = task else {
            return Ok(());
        };
        let inserted = tx.execute(
            "INSERT INTO evidence_collections(attempt_id,relative_prefix,source_path,source_tree,state)
             SELECT ?1,?2,?3,?4,'collecting' WHERE NOT EXISTS (
                 SELECT 1 FROM evidence_collections WHERE attempt_id=?1 AND relative_prefix=?2 AND source_path=?3
             )",
            params![
                collection.attempt,
                collection.prefix,
                collection.source.as_os_str().as_bytes(),
                collection.tree,
            ],
        )?;
        if inserted != 0 {
            tx.execute(
                "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
                 VALUES (?1,?2,?3,?4,'evidence.deferred','warning',?5)",
                params![self.project_id, task, collection.attempt, now_ms(), note],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Raise the artifact limits the evidence has steadily grown past.
    fn extend_for(&self, attempt: i64, pending: &[&Found]) -> Result<()> {
        let sizes = pending.iter().map(|found| found.size);
        if let Some(largest) = sizes.clone().max() {
            self.extend_steadily(attempt, Limit::ArtifactFileBytes, largest)?;
        }
        let file_limit = effective_limit(self.db.connection(), attempt, Limit::ArtifactFileBytes)?;
        let (files, bytes) = retained(self.db.connection(), attempt, false)?;
        let fitting = sizes.filter(|&size| size <= file_limit);
        let more = fitting.clone().fold(0_u64, u64::saturating_add);
        self.extend_steadily(
            attempt,
            Limit::ArtifactAttemptBytes,
            bytes.saturating_add(more),
        )?;
        self.extend_steadily(
            attempt,
            Limit::ArtifactAttemptFiles,
            files.saturating_add(fitting.count() as u64),
        )?;
        Ok(())
    }

    /// Archive what the limits allow, in order. When anything is left out, list every file
    /// with its size and checksum and mark the evidence truncated. Returns that note.
    pub(crate) fn collect_evidence(&self, collection: &Collection<'_>) -> Result<Option<String>> {
        let id = self.reserve_collection(collection)?;
        // A worker may have replaced its directory or locked it.
        let metadata = fs::symlink_metadata(collection.source)?;
        require(
            metadata.is_dir(),
            "The artifact directory was replaced; nothing could be read from it",
        )?;
        fs::set_permissions(collection.source, fs::Permissions::from_mode(0o700))?;
        self.reconcile_artifact_reservations()?;
        let found = list(collection.source)?;
        let published = self.published(collection.attempt, collection.prefix, id)?;
        let relative = |index: usize| format!("{}/file-{id}-{index}", collection.prefix);
        let pending: Vec<&Found> = found
            .iter()
            .enumerate()
            .filter(|(index, found)| {
                found.unusable.is_none() && !published.contains_key(&relative(*index))
            })
            .map(|(_, found)| found)
            .collect();
        self.extend_for(collection.attempt, &pending)?;
        let db = self.db.connection();
        let limit = |limit| effective_limit(db, collection.attempt, limit);
        let (file_limit, byte_limit, count_limit) = (
            limit(Limit::ArtifactFileBytes)?,
            limit(Limit::ArtifactAttemptBytes)?,
            limit(Limit::ArtifactAttemptFiles)?,
        );
        let (mut files, mut bytes) = retained(db, collection.attempt, false)?;
        let mut failures = 0_u32;
        let mut last_failure = String::new();
        let mut omitted: BTreeMap<usize, String> = BTreeMap::new();
        for (index, found) in found.iter().enumerate() {
            let target = relative(index);
            if published.contains_key(&target) {
                continue;
            }
            let reason = if let Some(why) = found.unusable {
                Some(why.to_owned())
            } else if found.size > file_limit {
                Some(format!("larger than artifact_file_bytes ({file_limit})"))
            } else if files >= count_limit {
                Some(format!("over artifact_attempt_files ({count_limit})"))
            } else if bytes.saturating_add(found.size) > byte_limit {
                Some(format!("over artifact_attempt_bytes ({byte_limit})"))
            } else if failures >= GIVE_UP {
                Some(format!(
                    "not archived after repeated failures: {last_failure}"
                ))
            } else {
                let name = found.name.to_string_lossy();
                match self.publish_artifact(
                    collection.attempt,
                    collection.token,
                    &name,
                    &collection.source.join(&found.name),
                    &target,
                    collection.tree,
                ) {
                    Ok(()) => {
                        failures = 0;
                        files += 1;
                        bytes = bytes.saturating_add(found.size);
                        None
                    }
                    Err(error) => {
                        failures += 1;
                        last_failure = match error {
                            Error::Conflict(message) => message,
                            Error::Breach(breach) => breach.reason,
                            Error::Capacity(refusal) => refusal.reason,
                            other => other.to_string(),
                        };
                        Some(last_failure.clone())
                    }
                }
            };
            if let Some(reason) = reason {
                omitted.insert(index, reason);
            }
        }
        let note = self.close_collection(id, collection, &found, &omitted)?;
        discard(collection.source)?;
        Ok(note)
    }

    /// The files an earlier, interrupted pass already published, with their checksums.
    fn published(
        &self,
        attempt: i64,
        prefix: &str,
        id: i64,
    ) -> Result<BTreeMap<String, Option<String>>> {
        let mut statement = self.db.connection().prepare(
            "SELECT relative_path,checksum_sha256 FROM artifacts WHERE attempt_id=?1 AND relative_path GLOB ?2",
        )?;
        let rows = statement
            .query_map(params![attempt, format!("{prefix}/file-{id}-*")], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(rows)
    }

    fn close_collection(
        &self,
        id: i64,
        collection: &Collection<'_>,
        found: &[Found],
        omitted: &BTreeMap<usize, String>,
    ) -> Result<Option<String>> {
        let db = self.db.connection();
        let total_bytes = found
            .iter()
            .map(|file| file.size)
            .fold(0_u64, u64::saturating_add);
        let archived_bytes = found
            .iter()
            .enumerate()
            .filter(|(index, _)| !omitted.contains_key(index))
            .fold(0_u64, |sum, (_, file)| sum.saturating_add(file.size));
        let note = (!omitted.is_empty()).then(|| {
            let reasons: BTreeSet<&str> = omitted.values().map(String::as_str).collect();
            let shown: Vec<_> = reasons.iter().take(3).copied().collect();
            let first = omitted
                .keys()
                .next()
                .map(|&index| found[index].name.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!(
                "Not archived from {first}: {}{}.",
                shown.join("; "),
                if reasons.len() > shown.len() {
                    "; and other reasons"
                } else {
                    ""
                }
            )
        });
        if note.is_some() {
            db.execute("DELETE FROM evidence_manifest WHERE collection_id=?1", [id])?;
            let published = self.published(collection.attempt, collection.prefix, id)?;
            for (batch, files) in found.chunks(BATCH).enumerate() {
                // Hash outside the write transaction.
                let rows: Vec<_> = files
                    .iter()
                    .enumerate()
                    .map(|(offset, file)| {
                        let index = batch * BATCH + offset;
                        let reason = omitted.get(&index);
                        let sum = match reason {
                            None => published
                                .get(&format!("{}/file-{id}-{index}", collection.prefix))
                                .cloned()
                                .flatten(),
                            Some(_) => checksum(&collection.source.join(&file.name)).ok(),
                        };
                        (index, file, sum, reason)
                    })
                    .collect();
                let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
                for (index, file, sum, reason) in rows {
                    tx.execute(
                        "INSERT INTO evidence_manifest(collection_id,ordinal,name,size_bytes,checksum_sha256,omitted) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![
                            id,
                            index as i64,
                            file.name.to_string_lossy(),
                            file.size as i64,
                            sum,
                            reason
                        ],
                    )?;
                }
                tx.commit()?;
            }
        }
        let archived = found.len() - omitted.len();
        let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE evidence_collections SET state=?2,total_files=?3,total_bytes=?4,archived_files=?5,archived_bytes=?6,note=?7 WHERE id=?1",
            params![
                id,
                if note.is_some() { "truncated" } else { "complete" },
                found.len() as i64,
                i64::try_from(total_bytes).unwrap_or(i64::MAX),
                archived as i64,
                i64::try_from(archived_bytes).unwrap_or(i64::MAX),
                note
            ],
        )?;
        let message = note.map(|note| {
            format!(
                "Evidence truncated: {archived} of {} files archived, {} not archived. {note}",
                found.len(),
                omitted.len()
            )
        });
        if let Some(message) = &message {
            truncation_event(&tx, collection.attempt, message)?;
        }
        tx.commit()?;
        Ok(message)
    }

    /// Mark a collection whose files cannot be read any more as truncated.
    fn abandon_collection(&self, id: i64, attempt: i64, why: &str) -> Result<String> {
        let note = format!("Evidence could not be archived: {why}.");
        let tx = Transaction::new_unchecked(self.db.connection(), TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE evidence_collections SET state='truncated',note=?2 WHERE id=?1 AND state='collecting'",
            params![id, note],
        )?;
        let message = format!("Evidence truncated. {note}");
        truncation_event(&tx, attempt, &message)?;
        tx.commit()?;
        Ok(message)
    }

    /// Finish the collections a guardian could not: from the retained files when they are
    /// still there, else recorded as truncated. The caller holds the worker lease, so no
    /// guardian is still collecting. Returns the notes to show.
    pub(crate) fn finish_pending_evidence(&self) -> Result<Vec<String>> {
        type Pending = (i64, i64, String, Vec<u8>, String, String);
        let pending: Vec<Pending> = self
            .db
            .connection()
            .prepare(
                "SELECT c.id,c.attempt_id,c.relative_prefix,c.source_path,c.source_tree,a.execution_token
                 FROM evidence_collections c JOIN attempts a ON a.id=c.attempt_id
                 WHERE a.project_id=?1 AND c.state='collecting' ORDER BY c.id",
            )?
            .query_map([self.project_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut notes = Vec::new();
        for (id, attempt, prefix, source, tree, token) in pending {
            let source = PathBuf::from(OsString::from_vec(source));
            let note = if source.is_dir() {
                let collection = Collection {
                    attempt,
                    token: &token,
                    prefix: &prefix,
                    source: &source,
                    tree: &tree,
                };
                match self.collect_evidence(&collection) {
                    Ok(note) => note,
                    Err(error) => {
                        let note = self.abandon_collection(id, attempt, &error.to_string())?;
                        let _ = discard(&source);
                        Some(note)
                    }
                }
            } else {
                Some(self.abandon_collection(id, attempt, "the retained files are gone")?)
            };
            notes.extend(note);
        }
        Ok(notes)
    }
}

fn truncation_event(db: &Connection, attempt: i64, message: &str) -> Result<()> {
    db.execute(
        "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
         SELECT project_id,task_id,id,?2,'evidence.truncated','warning',?3 FROM attempts WHERE id=?1",
        params![attempt, now_ms(), message],
    )?;
    Ok(())
}

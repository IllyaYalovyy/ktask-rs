//! Read an exact, retained check output range without opening a writable instance.
use super::{Error, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

pub(crate) const EXTENSION: &str = "ktask-output";

#[derive(Deserialize, Serialize)]
pub(crate) struct OutputReference {
    pub state: PathBuf,
    pub attempt: i64,
    pub step: i64,
    pub start: i64,
    pub end: i64,
}

impl OutputReference {
    pub fn read(&self, offset: u64, limit: usize) -> Result<Vec<u8>> {
        if !self.state.is_absolute() || self.start < 0 || self.end < self.start {
            return Err(Error::Conflict("Invalid check output reference".into()));
        }
        let mut db = Connection::open_with_flags(
            self.state.join("ktask.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.pragma_update(None, "trusted_schema", false)?;
        let identity = db.pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?;
        let tx = db.transaction()?;
        if identity != super::migrations::APPLICATION_ID
            || !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM attempt_steps WHERE id=?1 AND attempt_id=?2
                    AND role='check' AND state IN ('succeeded','failed'))",
                params![self.step, self.attempt],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(Error::Conflict(
                "Check output reference no longer identifies a recorded step".into(),
            ));
        }
        let rows: i64 = tx.query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1 AND sequence>?2 AND sequence<=?3",
            params![self.attempt, self.start, self.end],
            |row| row.get(0),
        )?;
        let removed_empty_range = rows == 0 && self.start == self.end && self.was_pruned(&tx)?;
        if rows != self.end - self.start || removed_empty_range {
            return self.archived(&tx, offset, limit);
        }
        self.page(&tx, offset, limit)
    }

    fn was_pruned(&self, db: &Connection) -> Result<bool> {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM evidence_cleanup_admissions WHERE attempt_id=?1)",
            [self.attempt],
            |row| row.get(0),
        )?)
    }

    fn page(&self, db: &Connection, mut skip: u64, limit: usize) -> Result<Vec<u8>> {
        let mut sizes = db.prepare(
            "SELECT sequence,length(CAST(content AS BLOB)) FROM attempt_output
             WHERE attempt_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence",
        )?;
        let mut rows = sizes.query(params![self.attempt, self.start, self.end])?;
        let mut slice = db.prepare(
            "SELECT substr(CAST(content AS BLOB),?3,?4) FROM attempt_output
             WHERE attempt_id=?1 AND sequence=?2",
        )?;
        let mut page = Vec::with_capacity(limit);
        while page.len() < limit
            && let Some(row) = rows.next()?
        {
            let size = row.get::<_, i64>(1)? as u64;
            if skip >= size {
                skip -= size;
                continue;
            }
            let sequence: i64 = row.get(0)?;
            let bytes: Vec<u8> = slice.query_row(
                params![
                    self.attempt,
                    sequence,
                    skip as i64 + 1,
                    (limit - page.len()) as i64
                ],
                |row| row.get(0),
            )?;
            page.extend(bytes);
            skip = 0;
        }
        Ok(page)
    }

    fn archived(&self, db: &Connection, offset: u64, limit: usize) -> Result<Vec<u8>> {
        let relative = format!("evidence/attempt-{}/step-{}.log", self.attempt, self.step);
        let archived: Option<(bool, i64)> = db
            .query_row(
                "SELECT pruned_at_ms IS NOT NULL OR pruning_requested_at_ms IS NOT NULL,size_bytes
             FROM artifacts WHERE attempt_id=?1 AND relative_path=?2",
                params![self.attempt, relative],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((false, size)) = archived else {
            let pruned = archived.is_some() || self.was_pruned(db)?;
            return Err(Error::Conflict(if pruned {
                "Check transcript output was pruned".into()
            } else {
                "The retained check output range is incomplete".into()
            }));
        };
        let expected = self.state.canonicalize()?.join(relative);
        let path = expected.canonicalize().map_err(|error| {
            Error::Conflict(format!(
                "The archived check transcript is unavailable ({error})"
            ))
        })?;
        if path != expected {
            return Err(Error::Conflict("Unsafe archived check output path".into()));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != size as u64 {
            return Err(Error::Conflict(
                "The archived check transcript is incomplete".into(),
            ));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut page = Vec::new();
        file.take(limit as u64).read_to_end(&mut page)?;
        Ok(page)
    }
}

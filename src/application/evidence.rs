//! Bounded read-only searches for the attempt evidence browser.
use super::output_text::{CHUNK_BYTES, read_output_range, visible_controls};
use super::*;
use std::io::{Read, Seek, SeekFrom};

const SEARCH_BYTES: usize = 8192;
const SEARCH_BATCH: usize = 64;

#[derive(Clone, Default, Debug)]
pub struct EvidenceCursor {
    pub sequence: i64,
    /// Raw byte offset, before any display-only control character escaping.
    pub offset: usize,
    pub carry: String,
    pub stream: String,
    pub stream_overlap: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct EvidenceMatch {
    pub page: usize,
    pub chunk: usize,
    pub sequence: i64,
    pub excerpt: String,
}

pub enum EvidenceSearch {
    Found(EvidenceMatch, EvidenceCursor),
    Continue(EvidenceCursor),
    Exhausted,
}

fn match_excerpt(text: &str, needle: &str) -> Option<(usize, String)> {
    let at = text
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())?;
    let start = text[..at]
        .char_indices()
        .rev()
        .nth(31)
        .map_or(0, |(i, _)| i);
    let end = text[at + needle.len()..]
        .char_indices()
        .nth(32)
        .map_or(text.len(), |(i, _)| at + needle.len() + i);
    Some((at, visible_controls(&text[start..end])))
}

fn retain_overlap(text: &str, needle: &str) -> String {
    let count = needle.chars().count().saturating_sub(1);
    let start = text
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    if count == 0 {
        String::new()
    } else {
        text[start..].to_owned()
    }
}

impl Service {
    /// Each call scans at most 512 KiB (plus a UTF-8 boundary). Continue on the next explicit Find.
    /// Each stream retains its own overlap across interleaved pipe records; system
    /// messages reset pipe overlap. The cursor is pinned to the output snapshot.
    pub fn search_output(
        &self,
        attempt: i64,
        watermark: i64,
        needle: &str,
        mut cursor: EvidenceCursor,
    ) -> Result<EvidenceSearch> {
        if needle.is_empty() {
            return Ok(EvidenceSearch::Exhausted);
        }
        let db = self.db.connection();
        let mut scanned = 0;
        for _ in 0..1024 {
            let row: Option<(i64, String, i64)> = db.query_row(
                "SELECT sequence,stream,length(CAST(content AS BLOB)) FROM attempt_output WHERE attempt_id=?1 AND sequence>=?2 AND sequence<=?3 ORDER BY sequence LIMIT 1",
                params![attempt,cursor.sequence,watermark],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
            ).optional()?;
            let Some((sequence, stream, length)) = row else {
                return Ok(EvidenceSearch::Exhausted);
            };
            let limit = SEARCH_BYTES.min(SEARCH_BYTES * SEARCH_BATCH - scanned);
            let slice = read_output_range(db, attempt, sequence, cursor.offset, limit)?;
            cursor.offset = slice.start;
            let content = slice.content;
            if stream == "system" && cursor.stream != "system" {
                cursor.stream_overlap.clear();
            }
            cursor.carry = cursor
                .stream_overlap
                .get(&stream)
                .cloned()
                .unwrap_or_default();
            cursor.stream = stream;
            let prefix = cursor.carry.len();
            let joined = format!("{}{content}", cursor.carry);
            if let Some((at, excerpt)) = match_excerpt(&joined, needle) {
                let position = (cursor.offset + at).saturating_sub(prefix);
                let end = (at + needle.len())
                    .saturating_sub(prefix)
                    .min(content.len());
                cursor.offset += end;
                cursor.sequence = sequence;
                cursor.carry.clear();
                cursor.stream_overlap.remove(&cursor.stream);
                if cursor.offset >= length as usize {
                    cursor.sequence += 1;
                    cursor.offset = 0;
                }
                let newer: i64 = db.query_row(
                    "SELECT count(*) FROM attempt_output WHERE attempt_id=?1 AND sequence>?2 AND sequence<=?3",
                    params![attempt,sequence,watermark], |r| r.get(0),
                )?;
                return Ok(EvidenceSearch::Found(
                    EvidenceMatch {
                        page: newer as usize / 80,
                        chunk: position / CHUNK_BYTES,
                        sequence,
                        excerpt,
                    },
                    cursor,
                ));
            }
            cursor.carry = retain_overlap(&joined, needle);
            cursor
                .stream_overlap
                .insert(cursor.stream.clone(), cursor.carry.clone());
            scanned += content.len();
            cursor.offset += content.len();
            cursor.sequence = sequence;
            if cursor.offset >= length as usize {
                cursor.sequence += 1;
                cursor.offset = 0;
            }
            if scanned >= SEARCH_BYTES * SEARCH_BATCH {
                break;
            }
        }
        Ok(EvidenceSearch::Continue(cursor))
    }

    pub fn artifact_id(&self, attempt: i64, index: usize) -> Result<Option<i64>> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT id FROM artifacts WHERE attempt_id=?1 ORDER BY id LIMIT 1 OFFSET ?2",
                params![attempt, index as i64],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Search only the selected retained transcript, with overlap across file pages.
    pub fn search_transcript(
        &self,
        attempt: i64,
        artifact: usize,
        root: &Path,
        needle: &str,
        mut cursor: EvidenceCursor,
    ) -> Result<EvidenceSearch> {
        if needle.is_empty() {
            return Ok(EvidenceSearch::Exhausted);
        }
        let relative: Option<String> = self.db.connection().query_row(
            "SELECT CASE WHEN pruned_at_ms IS NULL THEN relative_path ELSE NULL END FROM artifacts WHERE attempt_id=?1 ORDER BY id LIMIT 1 OFFSET ?2",
            params![attempt,artifact as i64], |r| r.get(0),
        ).optional()?.flatten();
        let Some(relative) = relative else {
            return Ok(EvidenceSearch::Exhausted);
        };
        let Some(path) = super::queries::retained_artifact_path(root, &relative) else {
            return Ok(EvidenceSearch::Exhausted);
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return Ok(EvidenceSearch::Exhausted);
        };
        file.seek(SeekFrom::Start(cursor.offset as u64))?;
        let mut bytes = vec![0; SEARCH_BYTES * SEARCH_BATCH];
        let count = file.read(&mut bytes)?;
        bytes.truncate(count);
        if count == 0 {
            return Ok(EvidenceSearch::Exhausted);
        }
        // Lossy display is also used for transcripts. Carry raw UTF-8 boundaries forward
        // so a split codepoint cannot hide a valid query in a retained UTF-8 log.
        let valid = match std::str::from_utf8(&bytes) {
            Ok(_) => count,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => count,
        };
        let content = String::from_utf8_lossy(&bytes[..valid]);
        let joined = format!("{}{content}", cursor.carry);
        if let Some((at, excerpt)) = match_excerpt(&joined, needle) {
            let position = cursor.offset.saturating_sub(cursor.carry.len()) + at;
            cursor.offset = position + needle.len();
            cursor.carry.clear();
            return Ok(EvidenceSearch::Found(
                EvidenceMatch {
                    page: position / 8192,
                    chunk: 0,
                    sequence: 0,
                    excerpt,
                },
                cursor,
            ));
        }
        cursor.offset += valid;
        cursor.carry = retain_overlap(&joined, needle);
        Ok(EvidenceSearch::Continue(cursor))
    }

    pub fn search_results(
        &self,
        attempt: i64,
        needle: &str,
        cursor: EvidenceCursor,
    ) -> Result<EvidenceSearch> {
        if needle.is_empty() {
            return Ok(EvidenceSearch::Exhausted);
        }
        let text = self.browse_results(attempt)?;
        let mut start = cursor.offset.min(text.len());
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        let Some((at, excerpt)) = match_excerpt(&text[start..], needle) else {
            return Ok(EvidenceSearch::Exhausted);
        };
        Ok(EvidenceSearch::Found(
            EvidenceMatch {
                page: 0,
                chunk: 0,
                sequence: 0,
                excerpt,
            },
            EvidenceCursor {
                offset: start + at + needle.len(),
                ..Default::default()
            },
        ))
    }
}

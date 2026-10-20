//! Byte-addressed, bounded access to stored output, including embedded NULs.
use super::*;
use rusqlite::Connection;

pub(super) const CHUNK_BYTES: usize = 512;

pub(super) struct OutputSlice {
    pub start: usize,
    pub content: String,
}

/// A codepoint belongs to the range containing its first byte. Read up to three
/// extra bytes to complete the final codepoint and skip any leading continuation.
/// SQLite TEXT length/substr stop at NUL, so both operations must use BLOBs.
pub(super) fn read_output_range(
    db: &Connection,
    attempt: i64,
    sequence: i64,
    offset: usize,
    limit: usize,
) -> Result<OutputSlice> {
    let bytes: Vec<u8> = db.query_row(
        "SELECT substr(CAST(content AS BLOB),?3,?4) FROM attempt_output WHERE attempt_id=?1 AND sequence=?2",
        params![attempt, sequence, offset.saturating_add(1) as i64, limit.saturating_add(3) as i64],
        |row| row.get(0),
    )?;
    let continuation = |byte: &u8| byte & 0xc0 == 0x80;
    let start = bytes.iter().take_while(|byte| continuation(byte)).count();
    let mut end = limit.min(bytes.len());
    while bytes.get(end).is_some_and(continuation) {
        end += 1;
    }
    let content = std::str::from_utf8(&bytes[start..end])
        .map_err(|error| Error::Incompatible(format!("Invalid UTF-8 in stored output: {error}")))?
        .to_owned();
    Ok(OutputSlice {
        start: offset.saturating_add(start),
        content,
    })
}

/// Preserve raw bytes for matching/cursors; make terminal controls visible only
/// when presenting evidence. Newlines and tabs retain their layout meaning.
pub(super) fn visible_controls(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            '\0'..='\u{1f}' => char::from_u32(0x2400 + character as u32).unwrap(),
            '\u{7f}' => '\u{2421}',
            _ => character,
        })
        .collect()
}

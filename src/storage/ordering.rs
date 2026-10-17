//! Canonical base-16 fractional strings, compared using SQLite BINARY collation.
//! Midpoint construction follows the fractional-digit algorithm (Rocicorp, CC0):
//! https://github.com/rocicorp/fractional-indexing. No floats or integer rank shifts.
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Error, Result};

const DIGITS: &[u8] = b"0123456789abcdef";

pub fn validate(key: &str) -> Result<()> {
    if key.is_empty() || key.ends_with('0') || !key.bytes().all(|c| DIGITS.contains(&c)) {
        return Err(Error::Conflict("invalid fractional order key".into()));
    }
    Ok(())
}

pub fn between(left: Option<&str>, right: Option<&str>) -> Result<String> {
    if let Some(key) = left {
        validate(key)?;
    }
    if let Some(key) = right {
        validate(key)?;
    }
    if left.zip(right).is_some_and(|(a, b)| a >= b) {
        return Err(Error::Conflict("order bounds are not increasing".into()));
    }
    let mut a = left.unwrap_or("").as_bytes();
    let mut b = right.map(str::as_bytes);
    let mut result = Vec::new();
    loop {
        if let Some(upper) = b {
            let common = upper
                .iter()
                .enumerate()
                .take_while(|(i, byte)| a.get(*i).copied().unwrap_or(b'0') == **byte)
                .count();
            if common > 0 {
                result.extend_from_slice(&upper[..common]);
                a = &a[common.min(a.len())..];
                b = Some(&upper[common..]);
                continue;
            }
        }
        let low = a
            .first()
            .map_or(0, |c| DIGITS.iter().position(|d| d == c).unwrap());
        let high = b.map_or(DIGITS.len(), |upper| {
            DIGITS.iter().position(|d| *d == upper[0]).unwrap()
        });
        if high - low > 1 {
            result.push(DIGITS[(low + high) / 2]);
            return Ok(String::from_utf8(result).expect("ASCII order key"));
        }
        if let Some(upper) = b.filter(|upper| upper.len() > 1) {
            result.push(upper[0]);
            return Ok(String::from_utf8(result).expect("ASCII order key"));
        }
        result.push(DIGITS[low]);
        a = &a[usize::from(!a.is_empty())..];
        b = None;
    }
}

/// Balanced batch allocation avoids the unnecessary growth of repeated append-midpoints.
pub fn batch(left: Option<&str>, right: Option<&str>, count: usize) -> Result<Vec<String>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let middle = between(left, right)?;
    let left_count = count / 2;
    let mut keys = batch(left, Some(&middle), left_count)?;
    keys.push(middle.clone());
    keys.extend(batch(Some(&middle), right, count - left_count - 1)?);
    Ok(keys)
}

/// Allocate and insert within the caller's SAME immediate transaction. Include historical
/// keys when finding the gap; retained history may occupy positions between visible rows.
pub fn keys_before(
    tx: &Transaction<'_>,
    project: i64,
    before: Option<i64>,
    count: usize,
) -> Result<Vec<String>> {
    let upper: Option<String> = match before {
        Some(id) => Some(tx.query_row(
            "SELECT order_key FROM tasks WHERE id = ?1 AND project_id = ?2 AND closed_at_ms IS NULL",
            params![id, project], |r| r.get(0),
        ).optional()?.ok_or_else(|| Error::Conflict("insertion target is not in this queue".into()))?),
        None => None,
    };
    let running: Option<String> = tx
        .query_row(
            "SELECT t.order_key FROM tasks t JOIN attempts a ON a.task_id = t.id WHERE t.project_id = ?1 AND a.state = 'running'",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    if upper
        .as_ref()
        .zip(running.as_ref())
        .is_some_and(|(u, r)| u <= r)
    {
        return Err(Error::Conflict(
            "cannot insert before running current work".into(),
        ));
    }
    let lower: Option<String> = tx.query_row(
        "SELECT max(order_key) FROM tasks WHERE project_id = ?1 AND (?2 IS NULL OR order_key < ?2)",
        params![project, upper],
        |r| r.get(0),
    )?;
    batch(lower.as_deref(), upper.as_deref(), count)
}

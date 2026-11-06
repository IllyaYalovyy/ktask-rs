//! Bounded file access without opening storage or taking a project lease.
use super::{Failure, Result};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};
const SCAN: usize = 1024 * 1024;
pub(super) fn read(path: &Path, offset: u64, limit: usize, search: Option<&str>) -> Result<Value> {
    if search.is_some_and(|s| s.is_empty() || s.len() > 512) {
        return Err(Failure::usage("Search must contain 1..512 UTF-8 bytes"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Failure::usage("Evidence must be a regular file"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let budget = if search.is_some() { SCAN } else { limit };
    let mut bytes = Vec::new();
    (&mut file).take(budget as u64).read_to_end(&mut bytes)?;
    if let Some(needle) = search {
        let mut matches = Vec::new();
        let mut next = offset;
        let mut used = 0;
        for (i, window) in bytes.windows(needle.len()).enumerate() {
            if window != needle.as_bytes() {
                continue;
            }
            let start = i.saturating_sub(96);
            let end = (i + needle.len() + 96).min(bytes.len());
            let mut excerpt = String::from_utf8_lossy(&bytes[start..end]).into_owned();
            if matches.is_empty() {
                while excerpt.len() > limit {
                    excerpt.pop();
                }
            }
            if !matches.is_empty() && used + excerpt.len() > limit {
                next = offset + i as u64;
                break;
            }
            used += excerpt.len();
            matches.push(json!({"offset":offset+i as u64,"text":excerpt}));
            next = offset + bytes.len() as u64;
        }
        if matches.is_empty() || next == offset + bytes.len() as u64 {
            next = offset + bytes.len() as u64;
            if next < meta.len() {
                next = next.saturating_sub((needle.len() - 1) as u64);
            }
        }
        Ok(
            json!({"file":path,"matches":matches,"scanned_bytes":bytes.len(),"next_offset":next,"eof":next>=meta.len()}),
        )
    } else {
        Ok(
            json!({"file":path,"offset":offset,"text":String::from_utf8_lossy(&bytes),"next_offset":offset+bytes.len() as u64,"eof":offset+bytes.len() as u64>=meta.len()}),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_and_search_find_early_and_cross_boundary_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut bytes = vec![b'x'; SCAN * 3];
        bytes[3..8].copy_from_slice(b"ERROR");
        bytes[SCAN - 2..SCAN + 3].copy_from_slice(b"ERROR");
        std::fs::write(&path, bytes).unwrap();
        let first = read(&path, 0, 4096, Some("ERROR")).unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(first["matches"][0]["offset"], 3);
        let second = read(
            &path,
            first["next_offset"].as_u64().unwrap(),
            4096,
            Some("ERROR"),
        )
        .unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(second["matches"][0]["offset"], SCAN - 2);
        let page = read(&path, 0, 100, None).unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(page["next_offset"], 100);
        assert_eq!(page["text"].as_str().unwrap().len(), 100);
        let tiny = read(&path, 0, 1, Some("ERROR")).unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(tiny["matches"][0]["text"].as_str().unwrap().len(), 1);
    }
}

//! Finite per-run storage budgets. Overrides are positive byte or entry counts.
use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

pub fn limit(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&value| value > 0 && value <= i64::MAX as u64)
        .unwrap_or(default)
}

pub fn scratch_exceeded(root: &Path, byte_limit: u64, entry_limit: u64) -> io::Result<bool> {
    let mut bytes = 0_u64;
    let mut entries = 0_u64;
    let mut directories = vec![root.to_path_buf()];
    while let Some(path) = directories.pop() {
        let children = match fs::read_dir(path) {
            Ok(children) => children,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in children {
            let entry = entry?;
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            entries = entries.saturating_add(1);
            let allocated = metadata.blocks().saturating_mul(512);
            bytes = bytes.saturating_add(if metadata.is_file() {
                metadata.len().max(allocated)
            } else {
                allocated
            });
            if bytes > byte_limit || entries > entry_limit {
                return Ok(true);
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok(false)
}

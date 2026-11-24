//! Finite per-run storage budgets. Overrides are positive byte or entry counts.
use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

pub fn limit(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&value| value > 0 && value <= i64::MAX as u64)
        .unwrap_or(default)
}

/// Entries that vanish or cannot be inspected are skipped: a supervised process may create,
/// delete or lock its own files (a test fixture with mode 000, for example) during the scan,
/// and that is no storage failure. A locked directory still counts as one entry.
pub fn scratch_exceeded(root: &Path, byte_limit: u64, entry_limit: u64) -> io::Result<bool> {
    let mut bytes = 0_u64;
    let mut entries = 0_u64;
    let mut directories = vec![root.to_path_buf()];
    while let Some(path) = directories.pop() {
        let children = match fs::read_dir(path) {
            Ok(children) => children,
            Err(error) if skippable(&error) => continue,
            Err(error) => return Err(error),
        };
        for entry in children {
            let entry = entry?;
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if skippable(&error) => continue,
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

fn skippable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn scan_skips_directories_the_owner_locked() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let locked = root.path().join("a/locked");
        let unsearchable = root.path().join("unsearchable");
        fs::create_dir_all(&locked)?;
        fs::create_dir_all(&unsearchable)?;
        fs::write(locked.join("hidden"), [0_u8; 64])?;
        fs::write(unsearchable.join("file"), [0_u8; 64])?;
        fs::write(root.path().join("seen"), [0_u8; 64])?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        // Listable, but its children cannot be inspected.
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o400))?;

        let within = scratch_exceeded(root.path(), 1024 * 1024, 100);
        // a, a/locked, unsearchable and seen: the fifth entry is out of reach.
        let over_entries = scratch_exceeded(root.path(), 1024 * 1024, 3);
        let at_entries = scratch_exceeded(root.path(), 1024 * 1024, 4);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o700))?;
        assert!(!within?);
        assert!(over_entries?);
        assert!(!at_entries?);
        Ok(())
    }
}

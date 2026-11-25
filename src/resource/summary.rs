//! A bounded account of what filled a scratch directory, taken before it is deleted.
use super::normalized;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs, io,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Entries examined before the account is declared partial.
const SCAN_ENTRIES: u64 = 1_000_000;
/// Files kept for the duplicate search, and the bytes it may read.
const DUPLICATE_CANDIDATES: usize = 200_000;
const HASH_BYTES: u64 = 512 * 1024 * 1024;
/// Directories are accounted down to this depth; deeper entries count toward their ancestor.
const DIRECTORY_DEPTH: usize = 3;
const LISTED: usize = 5;
const TEXT_LIMIT: usize = 2048;

/// Where the scratch bytes and entries went.
#[derive(Debug, Default)]
pub struct ScratchSummary {
    pub bytes: u64,
    pub entries: u64,
    /// The scan stopped before the whole tree was seen.
    pub partial: bool,
    pub files: Vec<(String, u64)>,
    /// (path, bytes, entries) of the largest directories by bytes, then by entries.
    pub directories: Vec<(String, u64, u64)>,
    pub duplicate_files: u64,
    pub duplicate_bytes: u64,
    /// The duplicate search covered every candidate within its read budget.
    pub duplicates_complete: bool,
}

#[derive(Default)]
struct Directory {
    bytes: u64,
    entries: u64,
}

struct Scan {
    summary: ScratchSummary,
    files: Vec<(PathBuf, u64, u64)>,
    directories: HashMap<PathBuf, Directory>,
}

/// Measures `root` the way the scratch limit does and names its biggest contributors.
pub fn summarize(root: &Path) -> ScratchSummary {
    let mut scan = Scan {
        summary: ScratchSummary {
            duplicates_complete: true,
            ..ScratchSummary::default()
        },
        files: Vec::new(),
        directories: HashMap::new(),
    };
    walk(root, &mut scan);
    finish(root, scan)
}

fn walk(root: &Path, scan: &mut Scan) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        // Entries a supervised process removed or locked meanwhile are skipped, as in the limit scan.
        let Ok(children) = fs::read_dir(&path) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(metadata) = fs::symlink_metadata(child.path()) else {
                continue;
            };
            if scan.summary.entries >= SCAN_ENTRIES {
                scan.summary.partial = true;
                return;
            }
            let allocated = metadata.blocks().saturating_mul(512);
            let size = if metadata.is_file() {
                metadata.len().max(allocated)
            } else {
                allocated
            };
            scan.summary.entries += 1;
            scan.summary.bytes = scan.summary.bytes.saturating_add(size);
            account(root, &child.path(), size, scan);
            if metadata.is_file() {
                if metadata.len() > 0 && scan.files.len() < DUPLICATE_CANDIDATES {
                    scan.files.push((child.path(), metadata.len(), size));
                } else if metadata.len() > 0 {
                    scan.summary.duplicates_complete = false;
                }
                let name = relative(root, &child.path());
                scan.summary.files.push((name, size));
                // Only the biggest few are ever listed.
                if scan.summary.files.len() > 4 * LISTED {
                    scan.summary
                        .files
                        .sort_by_key(|file| std::cmp::Reverse(file.1));
                    scan.summary.files.truncate(LISTED);
                }
            } else if metadata.is_dir() {
                pending.push(child.path());
            }
        }
    }
}

/// Charges an entry to each of its ancestor directories within the accounted depth.
fn account(root: &Path, path: &Path, size: u64, scan: &mut Scan) {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut ancestor = PathBuf::new();
    let parents = relative.components().count().saturating_sub(1);
    for component in relative.components().take(parents.min(DIRECTORY_DEPTH)) {
        ancestor.push(component);
        let directory = scan.directories.entry(ancestor.clone()).or_default();
        directory.bytes = directory.bytes.saturating_add(size);
        directory.entries += 1;
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn finish(root: &Path, mut scan: Scan) -> ScratchSummary {
    let mut summary = std::mem::take(&mut scan.summary);
    summary.files.sort_by_key(|file| std::cmp::Reverse(file.1));
    summary.files.truncate(LISTED);
    let mut all: Vec<_> = scan.directories.into_iter().collect();
    all.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)));
    let mut listed: Vec<_> = all.drain(..all.len().min(LISTED)).collect();
    all.sort_by(|a, b| b.1.entries.cmp(&a.1.entries).then_with(|| a.0.cmp(&b.0)));
    listed.extend(all.into_iter().take(LISTED));
    for (path, directory) in listed {
        let path = relative(root, &root.join(path));
        if !summary.directories.iter().any(|(seen, _, _)| *seen == path) {
            summary
                .directories
                .push((path, directory.bytes, directory.entries));
        }
    }
    duplicates(&mut summary, scan.files);
    summary
}

/// Byte-identical files: files of equal length are hashed, largest first, within a read budget.
fn duplicates(summary: &mut ScratchSummary, files: Vec<(PathBuf, u64, u64)>) {
    let mut by_length: HashMap<u64, Vec<(PathBuf, u64)>> = HashMap::new();
    for (path, length, allocated) in files {
        by_length.entry(length).or_default().push((path, allocated));
    }
    let mut groups: Vec<_> = by_length
        .into_iter()
        .filter(|(_, group)| group.len() > 1)
        .collect();
    groups.sort_by_key(|group| std::cmp::Reverse(group.0));
    let mut budget = HASH_BYTES;
    for (length, group) in groups {
        let mut seen: HashMap<[u8; 32], u64> = HashMap::new();
        for (path, allocated) in group {
            if budget < length {
                summary.duplicates_complete = false;
                break;
            }
            budget -= length;
            let Ok(digest) = hash(&path) else {
                continue;
            };
            let count = seen.entry(digest).or_default();
            if *count > 0 {
                summary.duplicate_files += 1;
                summary.duplicate_bytes = summary.duplicate_bytes.saturating_add(allocated);
            }
            *count += 1;
        }
    }
}

fn hash(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..read]);
    }
}

impl ScratchSummary {
    /// What the supervisor compares between breaches: the directory, else the file, that
    /// holds most of the scratch, with digits replaced.
    pub fn cause(&self) -> String {
        self.directories
            .first()
            .map(|(path, _, _)| path)
            .or_else(|| self.files.first().map(|(path, _)| path))
            .map_or_else(|| "empty".to_owned(), |path| normalized(path))
    }

    /// At most 2 KiB for the remediation model.
    pub fn render(&self) -> String {
        let mut text = format!(
            "Scratch holds {} bytes in {} entries{}.",
            self.bytes,
            self.entries,
            if self.partial {
                " (scan stopped early; at least)"
            } else {
                ""
            }
        );
        if !self.files.is_empty() {
            text.push_str("\nLargest files: ");
            text.push_str(&join(
                self.files.iter().map(|(p, b)| format!("{p} ({b} bytes)")),
            ));
        }
        if !self.directories.is_empty() {
            text.push_str("\nLargest directories: ");
            text.push_str(&join(
                self.directories
                    .iter()
                    .map(|(p, b, n)| format!("{p} ({b} bytes, {n} entries)")),
            ));
        }
        text.push_str(&format!(
            "\nByte-identical files: {} duplicate(s) of another file, {} bytes{}.",
            self.duplicate_files,
            self.duplicate_bytes,
            if self.duplicates_complete {
                ""
            } else {
                " (at least; the search was bounded)"
            }
        ));
        if text.len() > TEXT_LIMIT {
            let mut end = TEXT_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        text
    }
}

fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_biggest_contributors_and_counts_identical_files() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let copy = root.path().join("project-copy/src");
        fs::create_dir_all(&copy)?;
        fs::create_dir_all(root.path().join("small"))?;
        for index in 0..4 {
            fs::write(copy.join(format!("same-{index}.rs")), vec![7_u8; 8192])?;
        }
        fs::write(copy.join("other.rs"), vec![9_u8; 8192])?;
        fs::write(root.path().join("small/note"), b"x")?;

        let summary = summarize(root.path());

        assert_eq!(summary.entries, 9);
        assert_eq!(summary.duplicate_files, 3);
        assert!(summary.duplicates_complete);
        assert_eq!(summary.directories[0].0, "project-copy");
        assert_eq!(summary.cause(), "project-copy");
        let text = summary.render();
        assert!(text.contains("project-copy/src"), "{text}");
        assert!(text.contains("3 duplicate(s)"), "{text}");
        assert!(text.len() <= TEXT_LIMIT);
        Ok(())
    }

    #[test]
    fn cause_ignores_numbered_names() {
        assert_eq!(normalized("build-17/x9"), normalized("build-18/x4"));
    }
}

//! The detached supervisor's runner log: two bounded segments, and the tail a failed
//! launch reports.
use std::{
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

/// Copies `input` into the log at `path`, keeping it and its `.log.1` predecessor within
/// `limit` bytes each.
pub(crate) fn rotate_log(mut input: impl Read, path: &Path, limit: u64) -> std::io::Result<()> {
    const MARKER: &[u8] = b"[runner log rotated; earlier output is in .log.1]\n";
    let previous = path.with_extension("log.1");
    if previous.exists() {
        let mut old = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&previous)?;
        trim_log(
            &mut old,
            limit,
            b"[runner log trimmed after limit change]\n",
        )?;
    }
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    trim_log(&mut log, limit, MARKER)?;
    log.seek(SeekFrom::End(0))?;
    let mut size = log.metadata()?.len();
    let mut buffer = [0_u8; 4096];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if size.saturating_add(n as u64) > limit {
            drop(log);
            let _ = std::fs::remove_file(&previous);
            std::fs::rename(path, &previous)?;
            log = std::fs::File::create(path)?;
            log.write_all(MARKER)?;
            size = log.metadata()?.len();
        }
        log.write_all(&buffer[..n])?;
        size += n as u64;
    }
    log.flush()
}

fn trim_log(log: &mut std::fs::File, limit: u64, marker: &[u8]) -> std::io::Result<()> {
    let size = log.metadata()?.len();
    if size <= limit {
        return Ok(());
    }
    let keep = limit.saturating_sub(marker.len() as u64);
    let mut source = size - keep;
    let mut target = marker.len() as u64;
    log.seek(SeekFrom::Start(0))?;
    log.write_all(marker)?;
    let mut buffer = [0_u8; 4096];
    while source < size {
        let n = (size - source).min(buffer.len() as u64) as usize;
        log.seek(SeekFrom::Start(source))?;
        log.read_exact(&mut buffer[..n])?;
        log.seek(SeekFrom::Start(target))?;
        log.write_all(&buffer[..n])?;
        source += n as u64;
        target += n as u64;
    }
    log.set_len(target)?;
    Ok(())
}

/// How far a log file reached when a launch began: a later read reports only what that
/// launch appended, never old errors, unless the file was replaced or rotated meanwhile.
#[derive(Clone, Copy)]
pub(super) struct Mark {
    device: u64,
    inode: u64,
    offset: u64,
}

impl Mark {
    /// None when the log does not exist yet.
    pub(super) fn of(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                offset: metadata.len(),
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// At most the last 8 KiB the log gained since `start`.
pub(super) fn tail_since(path: &Path, start: Option<Mark>) -> std::io::Result<String> {
    const TAIL: u64 = 8192;
    let mut log = std::fs::File::open(path)?;
    let metadata = log.metadata()?;
    let end = metadata.len();
    let start = start
        .filter(|mark| {
            mark.device == metadata.dev() && mark.inode == metadata.ino() && mark.offset <= end
        })
        .map_or(end.saturating_sub(TAIL), |mark| {
            mark.offset.max(end.saturating_sub(TAIL))
        });
    log.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    log.take(TAIL).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    #[test]
    fn detached_log_keeps_only_two_bounded_segments() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("runner-1.log");
        std::fs::write(&path, vec![b'p'; 8190])?;
        rotate_log(std::io::Cursor::new(vec![b'x'; 100_000]), &path, 8192)?;
        let previous = path.with_extension("log.1");
        assert!(std::fs::metadata(&path)?.len() <= 8192 + 4096);
        assert!(std::fs::metadata(&previous)?.len() <= 8192 + 4096);
        assert!(std::fs::read_to_string(&path)?.contains("runner log rotated"));
        Ok(())
    }

    #[test]
    fn lowering_log_limit_compacts_existing_segments() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("runner-1.log");
        let previous = path.with_extension("log.1");
        std::fs::write(&path, vec![b'a'; 30_000])?;
        std::fs::write(&previous, vec![b'b'; 30_000])?;
        rotate_log(std::io::Cursor::new(b"new"), &path, 8192)?;
        assert!(std::fs::metadata(&path)?.len() <= 8192 + 4096);
        assert!(std::fs::metadata(&previous)?.len() <= 8192 + 4096);
        assert!(std::fs::read_to_string(&path)?.ends_with("new"));
        Ok(())
    }
}

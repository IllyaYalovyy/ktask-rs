use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
};

use tempfile::{Builder, TempDir};

/// Owns disposable files for one run or test. A kernel lock distinguishes live owners
/// from crashed ones without trusting PIDs or deleting another live run's files.
pub struct Scratch {
    dir: Option<TempDir>,
    _lease: File,
    sweep: File,
}

impl Scratch {
    /// Abandoned scratch is deleted before this returns.
    pub fn new(root: &Path) -> io::Result<Self> {
        let (scratch, abandoned) = Self::claim(root)?;
        delete_all(&abandoned);
        Ok(scratch)
    }

    /// For interactive startup: abandoned scratch, which can hold many thousands of
    /// files, is deleted in the background so the screen appears at once.
    pub fn new_in_background(root: &Path) -> io::Result<Self> {
        let (scratch, abandoned) = Self::claim(root)?;
        std::thread::spawn(move || delete_all(&abandoned));
        Ok(scratch)
    }

    /// Deletes every run directory whose owner is gone and reports how many. Live owners
    /// keep their kernel lease, so their files are never touched.
    pub fn reclaim(root: &Path) -> io::Result<usize> {
        match fs::metadata(root) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        }
        let sweep = open_lock(&root.join(".sweep.lock"))?;
        sweep.lock()?;
        let abandoned = reclaimed_entries(root).and_then(|mut found| {
            found.extend(set_aside_abandoned(root)?);
            Ok(found)
        });
        sweep.unlock()?;
        let abandoned = abandoned?;
        delete_all(&abandoned);
        Ok(abandoned.len())
    }

    fn claim(root: &Path) -> io::Result<(Self, Vec<PathBuf>)> {
        fs::create_dir_all(root)?;
        let sweep = open_lock(&root.join(".sweep.lock"))?;
        sweep.lock()?;
        // Creation and sweeping share this lock, so an owner cannot be collected
        // in the short interval between creating its directory and locking its lease.
        // Both ordinary workers and the interactive process retry interrupted deletion.
        // Collect these under the sweep lock so a newly renamed run is included once.
        let mut abandoned = reclaimed_entries(root)?;
        abandoned.extend(set_aside_abandoned(root)?);
        let dir = Builder::new().prefix("run-").tempdir_in(root)?;
        let lease = open_lock(&dir.path().join(".lease"))?;
        lease.lock()?;
        sweep.unlock()?;
        let scratch = Self {
            dir: Some(dir),
            _lease: lease,
            sweep,
        };
        Ok((scratch, abandoned))
    }

    pub fn path(&self) -> &Path {
        self.dir.as_ref().expect("open scratch directory").path()
    }

    /// Normal exits report cleanup errors; unwinding also gets TempDir's fallback cleanup.
    pub fn close(mut self) -> io::Result<()> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        // Removing .lease and its directory must be atomic with respect to the
        // sweeper: otherwise it can recreate .lease during TempDir::close.
        self.sweep.lock()?;
        let result = self.dir.take().expect("open scratch directory").close();
        let unlocked = self.sweep.unlock();
        result.and(unlocked)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("scratch cleanup failed: {error}");
        }
    }
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

const RECLAIMED: &str = ".reclaimed-";

fn reclaimed_entries(root: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(entries(root, RECLAIMED)?
        .into_iter()
        .filter(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let Some((owner, run)) = name
                .strip_prefix(RECLAIMED)
                .and_then(|name| name.split_once("-run-"))
            else {
                return false;
            };
            owner
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                && !run.is_empty()
                && run.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .collect())
}

fn entries(root: &Path, prefix: &str) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(prefix) && entry.file_type()?.is_dir() {
            found.push(entry.path());
        }
    }
    Ok(found)
}

/// Renames each run directory whose owner is gone out of the run namespace. A rename is
/// instant, so the sweep lock is never held while thousands of files are deleted.
fn set_aside_abandoned(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut abandoned = Vec::new();
    for path in entries(root, "run-")? {
        let lease = match open_lock(&path.join(".lease")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match lease.try_lock() {
            Ok(()) => {
                let name = path
                    .file_name()
                    .expect("run directory name")
                    .to_string_lossy();
                let mut sequence = 0_u64;
                let aside = loop {
                    let candidate = root.join(format!(
                        "{RECLAIMED}{}-{sequence}-{name}",
                        std::process::id()
                    ));
                    match fs::symlink_metadata(&candidate) {
                        Ok(_) => sequence += 1,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => break candidate,
                        Err(error) => return Err(error),
                    }
                };
                match fs::rename(&path, &aside) {
                    Ok(()) => abandoned.push(aside),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
    Ok(abandoned)
}

/// Best effort: report failures and leave the original name for the next startup.
fn delete_all(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = fs::remove_dir_all(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("scratch reclamation failed for {}: {error}", path.display());
        }
    }
}

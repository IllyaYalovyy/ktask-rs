use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
};

use tempfile::{Builder, TempDir};

pub(crate) const LEASE_NAME: &str = ".lease";

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
        let (scratch, abandoned) = Self::claim(root, None)?;
        delete_all(&abandoned);
        Ok(scratch)
    }

    /// Like [`Self::new`] with a chosen directory name starting `run-`, for tests whose
    /// recorded screens show its path. Fails with `AlreadyExists` while another owns it.
    pub fn named(root: &Path, name: &str) -> io::Result<Self> {
        let (scratch, abandoned) = Self::claim(root, Some(name))?;
        delete_all(&abandoned);
        Ok(scratch)
    }

    /// For interactive startup: abandoned scratch, which can hold many thousands of
    /// files, is deleted in the background so the screen appears at once.
    pub fn new_in_background(root: &Path) -> io::Result<Self> {
        let (scratch, abandoned) = Self::claim(root, None)?;
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

    fn claim(root: &Path, name: Option<&str>) -> io::Result<(Self, Vec<PathBuf>)> {
        fs::create_dir_all(root)?;
        let sweep = open_lock(&root.join(".sweep.lock"))?;
        sweep.lock()?;
        // Creation and sweeping share this lock, so an owner cannot be collected
        // in the short interval between creating its directory and locking its lease.
        // Both ordinary workers and the interactive process retry interrupted deletion.
        // Collect these under the sweep lock so a newly renamed run is included once.
        let mut abandoned = reclaimed_entries(root)?;
        abandoned.extend(set_aside_abandoned(root)?);
        let dir = match name {
            Some(name) => Builder::new().prefix(name).rand_bytes(0).tempdir_in(root),
            None => Builder::new().prefix("run-").tempdir_in(root),
        };
        let dir = match dir {
            Ok(dir) => dir,
            Err(error) => {
                sweep.unlock()?;
                delete_all(&abandoned);
                return Err(error);
            }
        };
        let lease = open_lock(&dir.path().join(LEASE_NAME))?;
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

    /// Pass a duplicate lease into a supervised child so a killed guardian cannot
    /// reclaim staging while that child still owns an open process tree.
    pub fn child_lease(&self) -> io::Result<File> {
        self._lease.try_clone()
    }

    /// Normal exits report cleanup errors; unwinding also gets TempDir's fallback cleanup.
    pub fn close(mut self) -> io::Result<()> {
        self.cleanup()
    }

    /// Keep an owned directory when an external writer could still be alive.
    /// The next owner must recover that writer before sweeping abandoned files.
    pub(crate) fn preserve(mut self) -> PathBuf {
        self.dir.take().expect("open scratch directory").keep()
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        // Removing .lease and its directory must be atomic with respect to the
        // sweeper: otherwise it can recreate .lease during TempDir::close.
        self.sweep.lock()?;
        let dir = self.dir.take().expect("open scratch directory");
        // A stopped process may leave directories it locked, which TempDir cannot remove.
        let result = remove_tree(dir.path()).and_then(|()| match dir.close() {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        });
        let unlocked = self.sweep.unlock();
        result.and(unlocked)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            note!("scratch cleanup failed: {error}");
        }
    }
}

pub(crate) fn open_lock(path: &Path) -> io::Result<File> {
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
        let lease = match open_lock(&path.join(LEASE_NAME)) {
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
        if let Err(error) = remove_tree(path) {
            note!("scratch reclamation failed for {}: {error}", path.display());
        }
    }
}

/// Removes a scratch tree even when a stopped process left directories without owner
/// permissions (a test fixture with mode 000, for example). A missing tree is success.
pub(crate) fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            unlock_directories(path)?;
            fs::remove_dir_all(path)
        }
        other => other,
    }
    .or_else(|error| match error.kind() {
        io::ErrorKind::NotFound => Ok(()),
        _ => Err(error),
    })
}

/// Gives the owner full access to every directory below `root`, never through a link.
fn unlock_directories(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut directories = vec![root.to_path_buf()];
    while let Some(path) = directories.pop() {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                if metadata.permissions().mode() & 0o700 != 0o700 {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                }
            }
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn locked_tree(run: &Path) -> io::Result<()> {
        let locked = run.join("fixture/a/locked");
        fs::create_dir_all(&locked)?;
        fs::write(locked.join("hidden"), b"x")?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        fs::set_permissions(run.join("fixture"), fs::Permissions::from_mode(0o500))
    }

    #[test]
    fn close_removes_directories_a_process_left_locked() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let scratch = Scratch::new(root.path())?;
        let run = scratch.path().to_path_buf();
        locked_tree(&run)?;
        scratch.close()?;
        assert!(!run.exists());
        Ok(())
    }

    #[test]
    fn reclaim_removes_abandoned_runs_with_locked_directories() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let run = Scratch::new(root.path())?.preserve();
        locked_tree(&run)?;
        // A link to a locked directory outside the run must leave it untouched.
        let outside = tempfile::tempdir()?;
        let kept = outside.path().join("kept");
        fs::create_dir(&kept)?;
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o000))?;
        std::os::unix::fs::symlink(&kept, run.join("link"))?;

        let reclaimed = Scratch::reclaim(root.path());
        let mode = fs::symlink_metadata(&kept)?.permissions().mode() & 0o777;
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o700))?;
        assert_eq!(reclaimed?, 1);
        assert_eq!(mode, 0o000);
        assert!(entries(root.path(), "run-")?.is_empty());
        assert!(entries(root.path(), RECLAIMED)?.is_empty());
        Ok(())
    }
}

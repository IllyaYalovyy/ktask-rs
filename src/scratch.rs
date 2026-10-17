use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
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
    pub fn new(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let sweep = open_lock(&root.join(".sweep.lock"))?;
        sweep.lock()?;
        // Creation and sweeping share this lock, so an owner cannot be collected
        // in the short interval between creating its directory and locking its lease.
        reap_abandoned(root)?;
        let dir = Builder::new().prefix("run-").tempdir_in(root)?;
        let lease = open_lock(&dir.path().join(".lease"))?;
        lease.lock()?;
        sweep.unlock()?;
        Ok(Self {
            dir: Some(dir),
            _lease: lease,
            sweep,
        })
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

fn reap_abandoned(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("run-") || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        let lease = match open_lock(&path.join(".lease")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match lease.try_lock() {
            Ok(()) => match fs::remove_dir_all(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
    Ok(())
}

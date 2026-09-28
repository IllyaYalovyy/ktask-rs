//! The run lock, kept in a file next to a project's journal and taken with `flock`.
//!
//! `flock` is held by the open file descriptor, at the kernel level: it is released the
//! moment the process holding it ends, for any reason, including a kill — so a run that is
//! killed mid-attempt never leaves the lock stuck for the next one.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ktask_core::{RunLock, RunLockError};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// The run lock for one project, backed by an exclusive, non-blocking `flock` on a file at
/// `path`. While it is held, the file holds the process id of the run that holds it, so a run
/// that fails to take the lock can name the one already running.
#[derive(Debug)]
pub struct FileRunLock {
    path: PathBuf,
    held: RefCell<Option<Flock<File>>>,
}

impl FileRunLock {
    /// A run lock backed by the file at `path`, not yet taken.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            held: RefCell::new(None),
        }
    }
}

/// Why `doing` failed against the lock file at `path`, as a [`RunLockError::Unusable`].
fn unusable(doing: &str, path: &Path, cause: impl std::fmt::Display) -> RunLockError {
    RunLockError::Unusable(format!("{doing} {}: {cause}", path.display()))
}

impl RunLock for FileRunLock {
    fn acquire(&self) -> Result<(), RunLockError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| unusable("cannot create the directory for the run lock", parent, e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            // Never truncated on open: a process that fails to lock still needs to read the
            // holder's process id back out of what is already there.
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| unusable("cannot open the run lock", &self.path, e))?;
        let mut locked = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => locked,
            Err((mut file, Errno::EAGAIN)) => {
                let mut content = String::new();
                let _ = file.read_to_string(&mut content);
                return Err(RunLockError::InProgress(content.trim().parse().ok()));
            }
            Err((_, errno)) => return Err(unusable("cannot lock", &self.path, errno)),
        };
        (|| {
            locked.set_len(0)?;
            locked.seek(SeekFrom::Start(0))?;
            write!(locked, "{}", std::process::id())?;
            locked.flush()
        })()
        .map_err(|e| unusable("cannot write the run lock", &self.path, e))?;
        *self.held.borrow_mut() = Some(locked);
        Ok(())
    }

    fn in_progress(&self) -> Result<bool, RunLockError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| unusable("cannot create the directory for the run lock", parent, e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| unusable("cannot open the run lock", &self.path, e))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            // Nobody held it: the trylock above just took it, and it is released again the
            // moment `locked` is dropped here.
            Ok(_locked) => Ok(false),
            Err((_, Errno::EAGAIN)) => Ok(true),
            Err((_, errno)) => Err(unusable("cannot lock", &self.path, errno)),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn an_unheld_lock_is_taken_and_records_this_process_in_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("run.lock");
        let lock = FileRunLock::new(path.clone());
        lock.acquire().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn a_lock_already_held_by_this_process_fails_naming_its_own_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.lock");
        let first = FileRunLock::new(path.clone());
        first.acquire().unwrap();

        let second = FileRunLock::new(path);
        let error = second.acquire().unwrap_err();
        assert_eq!(error, RunLockError::InProgress(Some(std::process::id())));
    }

    #[test]
    fn the_lock_is_free_again_once_the_holder_is_dropped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.lock");
        let first = FileRunLock::new(path.clone());
        first.acquire().unwrap();
        drop(first);

        let second = FileRunLock::new(path);
        second.acquire().unwrap();
    }

    #[test]
    fn in_progress_is_false_on_a_lock_nobody_has_ever_taken() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("run.lock");
        let lock = FileRunLock::new(path);
        assert_eq!(lock.in_progress(), Ok(false));
    }

    #[test]
    fn in_progress_is_true_while_another_holder_has_the_lock() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.lock");
        let holder = FileRunLock::new(path.clone());
        holder.acquire().unwrap();

        let peeker = FileRunLock::new(path);
        assert_eq!(peeker.in_progress(), Ok(true));
    }

    #[test]
    fn in_progress_is_false_again_once_the_holder_is_dropped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.lock");
        let holder = FileRunLock::new(path.clone());
        holder.acquire().unwrap();
        drop(holder);

        let peeker = FileRunLock::new(path);
        assert_eq!(peeker.in_progress(), Ok(false));
    }

    #[test]
    fn in_progress_does_not_itself_take_the_lock() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.lock");
        let peeker = FileRunLock::new(path.clone());
        assert_eq!(peeker.in_progress(), Ok(false));

        // Peeking left the lock free: a real acquire still succeeds afterwards.
        let acquirer = FileRunLock::new(path);
        acquirer.acquire().unwrap();
    }
}

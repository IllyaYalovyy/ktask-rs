//! Repository lock for serializing publication operations across processes.

use crate::Error;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Lock file marker that holds process id and start time.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct LockMarker {
    pid: u32,
    start_time: u64,
}

/// A repository lock that serializes publication operations.
///
/// The lock is acquired by creating a lock file with the current process's
/// PID and start time. If another process holds the lock with a live PID,
/// `acquire` will block until the timeout expires. If the lock holder's PID
/// is not alive, the lock is reclaimed.
#[derive(Debug)]
pub struct RepoLock {
    path: std::path::PathBuf,
}

impl RepoLock {
    /// Acquire a lock in the given directory with a timeout.
    ///
    /// Returns a `RepoLock` that will release the lock file on drop.
    /// If another process holds an active lock, blocks and retries until
    /// the timeout expires. If the lock holder's PID is not alive, the
    /// lock is reclaimed and logged as a reclamation.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock cannot be acquired within the timeout,
    /// or if I/O operations fail.
    pub fn acquire(dir: &Path, timeout: Duration) -> Result<RepoLock> {
        let lock_path = dir.join(".repo.lock");
        let start = std::time::Instant::now();
        let pid = process::id();
        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| {
                Error::Io(std::io::Error::other(format!(
                    "failed to get current time: {e}"
                )))
            })?
            .as_secs();

        loop {
            // Try to create lock file exclusively
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    // Successfully created lock file
                    let marker = LockMarker { pid, start_time };
                    let json = serde_json::to_string(&marker)?;
                    file.write_all(json.as_bytes())?;
                    return Ok(RepoLock { path: lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if handle_existing_lock(&lock_path, start, timeout)? {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }
}

/// Check if a process with the given PID is alive.
fn is_process_alive(pid: u32) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;

    let nix_pid = Pid::from_raw(pid.cast_signed());
    // Send signal 0 to check if process is alive without actually sending a signal
    signal::kill(nix_pid, None).is_ok()
}

/// Handle an existing lock file, returning whether to retry.
fn handle_existing_lock(
    lock_path: &Path,
    start: std::time::Instant,
    timeout: Duration,
) -> Result<bool> {
    if let Ok(content) = fs::read_to_string(lock_path) {
        if let Ok(marker) = serde_json::from_str::<LockMarker>(&content) {
            if is_process_alive(marker.pid) {
                if start.elapsed() >= timeout {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!(
                            "failed to acquire lock after {timeout:?}: process {pid} holds lock",
                            timeout = timeout,
                            pid = marker.pid
                        ),
                    )));
                }
                return Ok(true);
            }
            #[allow(clippy::print_stderr)]
            {
                eprintln!(
                    "Reclaimed repository lock held by dead process {pid}",
                    pid = marker.pid
                );
            }
            fs::remove_file(lock_path)?;
            return Ok(true);
        }
        let _ = fs::remove_file(lock_path);
        return Ok(true);
    }

    if start.elapsed() >= timeout {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("failed to acquire lock after {timeout:?}"),
        )));
    }
    Ok(true)
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_creates_lock_file() {
        let dir = TempDir::new().unwrap();
        let lock = RepoLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        let lock_path = dir.path().join(".repo.lock");
        assert!(lock_path.exists());
        drop(lock);
        assert!(!lock_path.exists());
    }

    #[test]
    fn acquire_lock_file_contains_pid_and_time() {
        let dir = TempDir::new().unwrap();
        let lock = RepoLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        let lock_path = dir.path().join(".repo.lock");
        let content = fs::read_to_string(&lock_path).unwrap();
        let marker: LockMarker = serde_json::from_str(&content).unwrap();
        assert_eq!(marker.pid, process::id());
        assert!(marker.start_time > 0);
        drop(lock);
    }

    #[test]
    fn second_acquire_blocks_until_first_released() {
        let dir = TempDir::new().unwrap();
        let lock1 = RepoLock::acquire(dir.path(), Duration::from_secs(5)).unwrap();

        // Try to acquire with a short timeout - should fail
        let result = RepoLock::acquire(dir.path(), Duration::from_millis(200));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to acquire lock"));

        drop(lock1);

        // Now acquisition should succeed
        let lock2 = RepoLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        drop(lock2);
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".repo.lock");

        // Create a stale lock file with an impossible PID (2^31 - 1)
        let marker = LockMarker {
            pid: 2_147_483_647,
            start_time: 0,
        };
        let json = serde_json::to_string(&marker).unwrap();
        fs::write(&lock_path, json).unwrap();

        // Acquire should succeed after reclaiming the stale lock
        let _lock = RepoLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        assert!(lock_path.exists());
    }

    #[test]
    fn corrupt_lock_file_is_reclaimed() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".repo.lock");

        // Create a corrupt lock file
        fs::write(&lock_path, "not valid json").unwrap();

        // Acquire should succeed after removing the corrupt lock
        let _lock = RepoLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        assert!(lock_path.exists());
    }
}

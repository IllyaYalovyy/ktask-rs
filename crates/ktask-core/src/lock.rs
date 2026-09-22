//! A cooperative, cross-process repository lock.
//!
//! [`acquire`] serializes integration and publication (VISION.md §10) across
//! every process working the same repository, including two invocations of
//! this binary racing each other. It works by creating a lock file
//! exclusively (`O_CREAT | O_EXCL`, so the filesystem — not a racy
//! check-then-create — is the arbiter of who wins), recording the winning
//! process's pid and OS-reported start time. A file left behind by a process
//! that has since died is reclaimed rather than waited out forever: the
//! start time, not just the pid, is what tells a dead holder apart from a
//! *different* process that the OS later handed the same pid.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// The lock file's name within the directory passed to [`acquire`].
const LOCK_FILE_NAME: &str = "repo.lock";

/// How long [`acquire`] sleeps between polls while waiting for a live
/// holder to release the lock.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The pid and start time recorded in a lock file, identifying its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// The process id that created the lock file.
    pub pid: u32,
    /// The holder's OS-reported process start time, in whatever platform
    /// unit `acquire` recorded it in (Linux: clock ticks since boot; other
    /// Unix targets cannot read this and always record zero). Only ever
    /// compared for equality against a fresh reading of the same pid, never
    /// interpreted as a duration or timestamp on its own.
    pub start_time: u64,
}

/// A held repository lock. Dropping it deletes the lock file, releasing the
/// lock for the next `acquire` — including one blocked waiting on it.
#[derive(Debug)]
pub struct RepoLock {
    path: PathBuf,
    reclaimed_from: Option<Holder>,
}

impl RepoLock {
    /// The previous holder this acquisition found dead and reclaimed the
    /// lock from, if the lock file already existed and its recorded holder
    /// was no longer running.
    #[must_use]
    pub fn reclaimed_from(&self) -> Option<Holder> {
        self.reclaimed_from
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Acquires the repository lock file `dir/repo.lock`, waiting up to
/// `timeout` for a live holder to release it.
///
/// A lock file whose recorded holder is no longer running (checked by pid
/// liveness, and on Linux additionally by comparing the OS process start
/// time) is reclaimed immediately rather than counting against `timeout`:
/// [`RepoLock::reclaimed_from`] reports the holder it took the lock from, so
/// the caller can log it.
///
/// # Errors
///
/// Returns [`Error::LockTimeout`] if a live holder still holds the lock
/// after `timeout` elapses, [`Error::Io`] if `dir` cannot be read or the
/// lock file cannot be created for a reason other than already existing,
/// and [`Error::Corrupt`] if an existing lock file's contents cannot be
/// parsed.
pub fn acquire(dir: &Path, timeout: Duration) -> Result<RepoLock> {
    let path = dir.join(LOCK_FILE_NAME);
    let deadline = Instant::now() + timeout;
    let me = Holder {
        pid: std::process::id(),
        start_time: current_start_time(std::process::id()).unwrap_or(0),
    };
    let mut reclaimed_from = None;

    loop {
        match create_exclusive(&path, &me) {
            Ok(()) => {
                return Ok(RepoLock {
                    path,
                    reclaimed_from,
                });
            }
            Err(Error::Io(err)) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }

        let Some(holder) = read_holder(&path)? else {
            // The file we just failed to create is already gone again
            // (its holder released it between our attempt and this read):
            // retry the create immediately.
            continue;
        };

        if !is_alive(&holder) {
            reclaim(&path)?;
            reclaimed_from = Some(holder);
            continue;
        }

        if Instant::now() >= deadline {
            return Err(Error::LockTimeout {
                path,
                timeout_secs: timeout.as_secs(),
                holder_pid: holder.pid,
            });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Removes a lock file found to be stale, tolerating a concurrent reclaim by
/// another waiter that got there first.
fn reclaim(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Creates `path` exclusively, failing with [`ErrorKind::AlreadyExists`]
/// wrapped in [`Error::Io`] if it already exists.
fn create_exclusive(path: &Path, holder: &Holder) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let contents = serde_json::to_string(holder)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Reads and parses the holder recorded in `path`, or `None` if it no
/// longer exists.
fn read_holder(path: &Path) -> Result<Option<Holder>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let holder = serde_json::from_str(&contents).map_err(|err| Error::Corrupt {
                detail: format!("lock file {} is not valid: {err}", path.display()),
            })?;
            Ok(Some(holder))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Whether `holder`'s process still appears to be the one running under its
/// recorded pid.
#[cfg(target_os = "linux")]
fn is_alive(holder: &Holder) -> bool {
    // A pid that no longer exists is obviously dead; one that exists but
    // whose start time no longer matches belongs to a different process the
    // OS later handed the same pid, which is just as dead a holder.
    current_start_time(holder.pid) == Some(holder.start_time)
}

/// Outside Linux there is no portable way to read another process's start
/// time, so only pid liveness is checked; a pid reused by an unrelated
/// process in the narrow window this leaves is a known limitation of this
/// fallback.
#[cfg(all(unix, not(target_os = "linux")))]
fn is_alive(holder: &Holder) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal;
    use nix::unistd::Pid;

    let pid = Pid::from_raw(i32::try_from(holder.pid).unwrap_or(i32::MAX));
    matches!(signal::kill(pid, None), Ok(()) | Err(Errno::EPERM))
}

/// `pid`'s start time in clock ticks since boot, read from `/proc`, or
/// `None` if `pid` is not currently running or `/proc` cannot be read.
#[cfg(target_os = "linux")]
fn current_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields 1 and 2 are the pid and the "(comm)" name; comm may itself
    // contain spaces or parens, so field 3 onward starts after the last
    // ')' rather than after a fixed split count. Start time is field 22,
    // i.e. index 19 once fields 1-3 are already behind us.
    let after_comm = stat.rfind(')')?;
    let rest = stat.get(after_comm + 2..)?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// No portable process start time outside Linux.
#[cfg(not(target_os = "linux"))]
fn current_start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_succeeds_immediately_when_no_lock_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");

        let lock = acquire(dir.path(), Duration::from_secs(1)).expect("acquire");

        assert!(dir.path().join(LOCK_FILE_NAME).is_file());
        assert_eq!(lock.reclaimed_from(), None);
    }

    #[test]
    fn the_lock_file_records_the_current_process_pid() {
        let dir = tempfile::tempdir().expect("tempdir");

        let _lock = acquire(dir.path(), Duration::from_secs(1)).expect("acquire");

        let contents = std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME)).expect("read");
        let holder: Holder = serde_json::from_str(&contents).expect("parse");
        assert_eq!(holder.pid, std::process::id());
    }

    #[test]
    fn dropping_a_repo_lock_deletes_its_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LOCK_FILE_NAME);

        let lock = acquire(dir.path(), Duration::from_secs(1)).expect("acquire");
        assert!(path.is_file());
        drop(lock);

        assert!(!path.exists());
    }

    #[test]
    fn a_second_acquire_blocks_then_times_out_with_a_clear_error_while_the_first_is_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _held = acquire(dir.path(), Duration::from_secs(1)).expect("first acquire");

        let timeout = Duration::from_millis(150);
        let started = Instant::now();
        let err = acquire(dir.path(), timeout).expect_err("must time out: still held");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= timeout,
            "must actually wait out the timeout, waited {elapsed:?}"
        );
        match &err {
            Error::LockTimeout {
                holder_pid,
                timeout_secs,
                ..
            } => {
                assert_eq!(*holder_pid, std::process::id());
                assert_eq!(*timeout_secs, 0);
            }
            other => panic!("expected LockTimeout, got {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains(&std::process::id().to_string()));
    }

    #[test]
    fn a_lock_whose_pid_is_not_alive_is_reclaimed_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dead_pid = spawn_and_wait_for_a_dead_pid();
        let stale = Holder {
            pid: dead_pid,
            start_time: 999,
        };
        std::fs::write(
            dir.path().join(LOCK_FILE_NAME),
            serde_json::to_string(&stale).expect("serialize"),
        )
        .expect("write stale lock");

        let lock = acquire(dir.path(), Duration::from_secs(1)).expect("must reclaim, not time out");

        assert_eq!(lock.reclaimed_from(), Some(stale));
        let contents =
            std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME)).expect("read new lock");
        let holder: Holder = serde_json::from_str(&contents).expect("parse");
        assert_eq!(holder.pid, std::process::id());
    }

    #[test]
    fn a_fresh_acquire_after_a_reclaim_reports_no_further_reclamation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dead_pid = spawn_and_wait_for_a_dead_pid();
        let stale = Holder {
            pid: dead_pid,
            start_time: 999,
        };
        std::fs::write(
            dir.path().join(LOCK_FILE_NAME),
            serde_json::to_string(&stale).expect("serialize"),
        )
        .expect("write stale lock");
        let first = acquire(dir.path(), Duration::from_secs(1)).expect("reclaim");
        assert!(first.reclaimed_from().is_some());
        drop(first);

        let second = acquire(dir.path(), Duration::from_secs(1)).expect("second acquire");

        assert_eq!(second.reclaimed_from(), None);
    }

    #[test]
    fn a_corrupt_lock_file_is_reported_as_corrupt_rather_than_silently_reclaimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(LOCK_FILE_NAME), b"not json").expect("write garbage");

        let err = acquire(dir.path(), Duration::from_millis(50)).expect_err("must fail");

        assert!(matches!(err, Error::Corrupt { .. }));
    }

    /// Spawns a trivial child process, waits for it to exit, and returns its
    /// pid: guaranteed not to be running any more, which is exactly the
    /// "holder is dead" case a stale lock file needs to exercise.
    fn spawn_and_wait_for_a_dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        let status = child.wait().expect("wait for child");
        assert!(status.success());
        pid
    }
}

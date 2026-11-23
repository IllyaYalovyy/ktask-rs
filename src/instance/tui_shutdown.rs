use std::{
    fs::{self, File, TryLockError},
    io,
    path::Path,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use super::Paths;
use crate::scratch::{LEASE_NAME, Scratch, open_lock};

const REQUEST: &str = "close";

// Keep the kernel lease until process exit, after normal terminal and scratch cleanup.
// The installer cannot replace the executable while any process still runs from it.
static PROCESS_LEASE: OnceLock<File> = OnceLock::new();

pub(crate) struct TuiSession(Scratch);

impl TuiSession {
    pub(crate) fn register(paths: &Paths) -> io::Result<Self> {
        let root = paths.runtime.join("tui");
        fs::create_dir_all(&root)?;
        let gate = open_lock(&root.join(".shutdown.lock"))?;
        match gate.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::other(
                    "Instance shutdown is in progress; reopen the TUI when it finishes",
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let session = Scratch::new_in_background(&root)?;
        PROCESS_LEASE
            .set(session.child_lease()?)
            .map_err(|_| io::Error::other("This process already registered a TUI session"))?;
        Ok(Self(session))
    }

    pub(crate) fn close_requested(&self) -> io::Result<bool> {
        self.0.path().join(REQUEST).try_exists()
    }

    pub(crate) fn close(self) -> io::Result<()> {
        self.0.close()
    }
}

/// The instance gate serializes shutdowns and prevents new TUI registrations
/// while shutdown is in progress. Existing windows own separate lifetime leases.
pub(crate) struct TuiShutdown {
    _gate: File,
    sessions: Vec<File>,
}

impl TuiShutdown {
    pub(crate) fn request(paths: &Paths) -> io::Result<Self> {
        let root = paths.runtime.join("tui");
        fs::create_dir_all(&root)?;
        let gate = open_lock(&root.join(".shutdown.lock"))?;
        wait_for_lock(
            || gate.try_lock(),
            Instant::now() + crate::runner::control::PATIENCE,
            "Timed out waiting for the TUI shutdown gate",
        )?;
        Scratch::reclaim(&root)?;
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("run-") {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => sessions.extend(request_close(&entry.path())?),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            _gate: gate,
            sessions,
        })
    }

    pub(crate) fn wait(&self) -> io::Result<()> {
        let deadline = Instant::now() + crate::runner::control::PATIENCE;
        for lease in &self.sessions {
            wait_for_lock(
                || lease.try_lock_shared(),
                deadline,
                "Timed out waiting for a TUI to close",
            )?;
        }
        Ok(())
    }
}

fn wait_for_lock(
    lock: impl Fn() -> Result<(), TryLockError>,
    deadline: Instant,
    timeout: &str,
) -> io::Result<()> {
    loop {
        match lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, timeout));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn request_close(path: &Path) -> io::Result<Option<File>> {
    let lease = match File::open(path.join(LEASE_NAME)) {
        Ok(lease) => lease,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match fs::write(path.join(REQUEST), []) {
        Ok(()) => Ok(Some(lease)),
        // The TUI can quit naturally while shutdown is taking its snapshot.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(lease)),
        Err(error) => Err(error),
    }
}

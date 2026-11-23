//! Instance-local ownership for supervisor startup, execution, and shutdown.
use crate::{
    instance::Paths,
    scratch::open_lock,
    storage::{Error, Result},
};
use std::{
    fs::{self, File, TryLockError},
    io,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Registration {
    admission: Option<File>,
    _lease: super::process::Lease,
}

impl Registration {
    pub(crate) fn start(paths: &Paths, project: i64) -> Result<Self> {
        let admission = gate(paths)?;
        match admission.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Conflict(
                    "Instance shutdown is in progress; run again when it finishes".into(),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        Ok(Self {
            admission: Some(admission),
            _lease: super::process::lock(&lease_path(paths, project))?,
        })
    }

    /// Shutdown can now see the running scheduler and request its pause.
    pub(crate) fn admitted(&mut self) {
        self.admission.take();
    }
}

pub(crate) fn close_admission(paths: &Paths, deadline: Instant) -> Result<File> {
    let gate = gate(paths)?;
    loop {
        match gate.try_lock() {
            Ok(()) => return Ok(gate),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out waiting for supervisor startup or another shutdown",
                )
                .into());
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

pub(crate) fn active(paths: &Paths, project: i64) -> Result<Option<File>> {
    let lease = match File::open(lease_path(paths, project)) {
        Ok(lease) => lease,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(super::process::inspect(&lease, |_| Ok(()))?
        .is_none()
        .then_some(lease))
}

/// Every registered project by name, and whether its runner is alive.
pub(crate) fn listed(
    paths: &Paths,
    db: &crate::storage::Database,
) -> Result<Vec<(crate::application::overview::Project, bool)>> {
    crate::application::overview::projects(db)?
        .into_iter()
        .map(|project| {
            let alive = active(paths, project.id)?.is_some();
            Ok((project, alive))
        })
        .collect()
}

/// The project's runner lease, held so no runner starts; None while one is alive.
pub(crate) struct Claim(#[allow(dead_code)] File);

pub(crate) fn claim(paths: &Paths, project: i64) -> Result<Option<Claim>> {
    fs::create_dir_all(paths.state.join("runners"))?;
    let lease = open_lock(&lease_path(paths, project))?;
    match lease.try_lock() {
        Ok(()) => Ok(Some(Claim(lease))),
        Err(TryLockError::WouldBlock) => {
            // A status check holds the lease shared for an instant; a runner holds it exclusively.
            thread::sleep(Duration::from_millis(50));
            match lease.try_lock() {
                Ok(()) => Ok(Some(Claim(lease))),
                Err(TryLockError::WouldBlock) => Ok(None),
                Err(TryLockError::Error(error)) => Err(error.into()),
            }
        }
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

pub(crate) fn lease_file(paths: &Paths, project: i64) -> PathBuf {
    lease_path(paths, project)
}

fn gate(paths: &Paths) -> io::Result<File> {
    let root = paths.state.join("runners");
    fs::create_dir_all(&root)?;
    open_lock(&root.join(".shutdown.lock"))
}

fn lease_path(paths: &Paths, project: i64) -> PathBuf {
    paths.state.join("runners").join(format!("{project}.lock"))
}

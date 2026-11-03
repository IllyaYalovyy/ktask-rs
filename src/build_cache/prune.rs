//! Generation pruning for one owned Cargo target directory: scan, plan, apply.
use super::{
    Policy,
    plan::{self, Generation, Totals},
    scan::{self, LINK_NOTE, OUTSIDE_NOTE, Skipped},
};
use crate::{
    instance::Paths,
    storage::{Error, Result},
};
use serde::Serialize;
use std::{
    fs::{self, File, TryLockError},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub root: PathBuf,
    /// Why nothing was done: an active build or another cleanup holds the cache.
    pub deferred: Option<String>,
    pub applied: bool,
    pub before_bytes: u64,
    pub before_files: u64,
    pub after_bytes: u64,
    pub after_files: u64,
    pub obsolete_generations: u64,
    pub removed_generations: u64,
    pub protected_generations: u64,
    pub freed_bytes: u64,
    pub freed_files: u64,
    /// Still above a limit although only protected or newest generations remain.
    pub over_capacity: bool,
    /// Paths below the root that were left alone (symbolic links are never followed).
    pub skipped: Vec<String>,
}

fn describe(mut skipped: Skipped) -> Vec<String> {
    skipped.sort();
    skipped.dedup();
    skipped
        .iter()
        .map(|(path, why)| format!("{}: {why}", path.display()))
        .collect()
}

/// Cargo holds `.cargo-lock` in each build directory while compiling. Holding them all
/// excludes a build that would start mid-cleanup; failing to take one means a build is live.
fn build_locks(
    root: &Path,
    profiles: &[PathBuf],
) -> io::Result<std::result::Result<Vec<File>, String>> {
    let mut held = Vec::new();
    let candidates = std::iter::once(root.to_path_buf()).chain(profiles.iter().cloned());
    for dir in candidates {
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(".cargo-lock"))
        else {
            continue;
        };
        match file.try_lock() {
            Ok(()) => held.push(file),
            Err(TryLockError::WouldBlock) => {
                return Ok(Err(format!("a build is active in {}", dir.display())));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(Ok(held))
}

/// Final guard before a removal: the entry's parent resolves inside the owned root and the
/// entry itself is not a link out of it. A vanished entry has nothing left to remove.
fn removable(root: &Path, path: &Path) -> bool {
    let Some((parent, name)) = path.parent().zip(path.file_name()) else {
        return false;
    };
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
        Err(_) => return false,
        Ok(meta) if meta.file_type().is_symlink() => return false,
        Ok(_) => {}
    }
    fs::canonicalize(parent).is_ok_and(|parent| {
        let resolved = parent.join(name);
        resolved.starts_with(root) && resolved != root
    })
}

/// Removes a generation's paths in scan order and returns what that freed and whether
/// every path is gone.
fn remove(root: &Path, generation: &Generation, skipped: &mut Skipped) -> (Totals, bool) {
    let mut freed = Totals::default();
    let mut complete = true;
    for (path, size) in &generation.paths {
        if !removable(root, path) {
            let why = if scan::is_link(path) {
                LINK_NOTE
            } else {
                OUTSIDE_NOTE
            };
            skipped.push((path.clone(), why));
            complete = false;
            continue;
        }
        let removed = if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match removed {
            Ok(()) => freed.add(*size),
            Err(e) if e.kind() == io::ErrorKind::NotFound => freed.add(*size),
            Err(_) => complete = false,
        }
    }
    (freed, complete)
}

/// Retires obsolete generations of an owned target directory, then evicts the oldest
/// others while it is over `policy`. The newest generations of every compilation unit,
/// final outputs, open or mapped files, the supervisor executable and fresh output stay.
pub fn prune(paths: &Paths, root: &Path, policy: &Policy, apply: bool) -> Result<Report> {
    let root = fs::canonicalize(root)?;
    if !super::is_owned(paths, &root) {
        return Err(Error::Conflict(format!(
            "{} is not owned by this instance; adopt it first",
            root.display()
        )));
    }
    let mut report = Report {
        root: root.clone(),
        applied: apply,
        ..Report::default()
    };
    let exclusive = crate::scratch::open_lock(&root.join(".ktask-cache.lock"))?;
    match exclusive.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            report.deferred = Some("another cleanup is running".into());
            return Ok(report);
        }
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    let mut skipped = Vec::new();
    let profiles = scan::profile_dirs(&root, &mut skipped);
    let _builds = match build_locks(&root, &profiles)? {
        Ok(held) => held,
        Err(reason) => {
            report.deferred = Some(reason);
            return Ok(report);
        }
    };
    let executable = fs::canonicalize(&paths.executable).unwrap_or(paths.executable.clone());
    let project = super::registered_project(paths, &root).unwrap_or_default();
    let scan = scan::scan(&root, &profiles, &project, &executable, &mut skipped);
    let plan = plan::plan(&scan.generations, policy, SystemTime::now(), scan.total);
    report.before_bytes = scan.total.bytes;
    report.before_files = scan.total.files;
    report.obsolete_generations = plan.obsolete;
    report.protected_generations = plan.protected;
    let mut after = plan.after;
    if apply {
        after = scan.total;
        for &index in &plan.retire {
            let (freed, complete) = remove(&root, &scan.generations[index], &mut skipped);
            after.sub(freed);
            report.removed_generations += u64::from(complete);
        }
    } else {
        report.removed_generations = plan.retire.len() as u64;
    }
    report.after_bytes = after.bytes;
    report.after_files = after.files;
    report.freed_bytes = scan.total.bytes - after.bytes;
    report.freed_files = scan.total.files - after.files;
    report.over_capacity = !after.within(policy);
    report.skipped = describe(skipped);
    Ok(report)
}

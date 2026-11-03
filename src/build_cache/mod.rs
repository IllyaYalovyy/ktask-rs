//! Ownership-aware lifecycle for development build caches and diagnostic trees.
//!
//! A directory is managed only when it carries a marker *and* this instance's registry
//! holds a matching registration for the same device and inode; a path alone is never
//! ownership. Everything else is inventoried, reported, and left alone until the owner
//! opts in with [`adopt`]. Provider session storage is inventoried separately and never
//! cleaned. Cleanup runs only where liveness is proved: no cargo build holds the profile
//! lock, no process has the file open, mapped or running, and the output is not fresh.
mod inventory;
mod plan;
mod prune;
mod scan;

pub use inventory::{Entry, Inventory, Kind, Ownership, inventory, size};
pub use prune::{Report, prune};

use crate::{
    instance::Paths,
    storage::{Error, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const MARKER: &str = ".ktask-cache-owner";
const POLICY_FILE: &str = "build-cache.json";
const REGISTRY: &str = "owned-caches";
const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// Finite per-instance bounds. `version` guards concurrent edits like other settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub version: u64,
    /// Allocated bytes one owned cache may hold after cleanup.
    pub max_bytes: u64,
    /// Files and directories one owned cache may hold after cleanup.
    pub max_files: u64,
    /// Newest generations of each local build artifact kept warm.
    pub keep_generations: u64,
    /// Output newer than this many seconds is treated as active compiler output.
    pub grace_secs: u64,
    /// Free space on the cache filesystem below which capacity pressure is reported.
    pub min_free_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: 0,
            max_bytes: 16 * GIB,
            max_files: 400_000,
            keep_generations: 3,
            grace_secs: 600,
            min_free_bytes: 8 * GIB,
        }
    }
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        let check = |name: &str, value: u64, low: u64, high: u64| {
            if (low..=high).contains(&value) {
                Ok(())
            } else {
                Err(Error::Conflict(format!(
                    "{name} must be between {low} and {high}"
                )))
            }
        };
        check("max bytes", self.max_bytes, 64 * MIB, 16 * 1024 * GIB)?;
        check("max files", self.max_files, 1_000, 50_000_000)?;
        check("keep generations", self.keep_generations, 1, 32)?;
        check("grace seconds", self.grace_secs, 0, 7 * 86_400)?;
        check("min free bytes", self.min_free_bytes, 0, 16 * 1024 * GIB)
    }
}

pub fn policy(paths: &Paths) -> Result<Policy> {
    match fs::read(paths.config.join(POLICY_FILE)) {
        Ok(bytes) => {
            let stored: Policy = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Incompatible(format!("{POLICY_FILE}: {e}")))?;
            stored
                .validate()
                .map_err(|e| Error::Incompatible(format!("{POLICY_FILE}: {e}")))?;
            Ok(stored)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
        Err(e) => Err(e.into()),
    }
}

/// Saves `next` if the stored policy still has `expected`'s version, and returns it with
/// the version advanced.
pub fn set_policy(paths: &Paths, expected: Policy, mut next: Policy) -> Result<Policy> {
    next.validate()?;
    fs::create_dir_all(&paths.config)?;
    let lock = crate::scratch::open_lock(&paths.config.join(".build-cache.lock"))?;
    lock.lock()?;
    let current = policy(paths)?;
    if current.version != expected.version {
        return Err(Error::Conflict(
            "Build cache policy changed; read it again".into(),
        ));
    }
    next.version = current.version + 1;
    write_atomic(
        &paths.config.join(POLICY_FILE),
        &serde_json::to_vec_pretty(&next).expect("policy JSON"),
    )?;
    Ok(next)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staged = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = File::create(&staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&staged, path)
}

#[derive(Serialize, Deserialize)]
struct Marker {
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Registration {
    token: String,
    root: PathBuf,
    dev: u64,
    ino: u64,
    project: PathBuf,
}

fn registry(paths: &Paths) -> PathBuf {
    paths.cache.join(REGISTRY)
}

/// Registrations whose directory still has the registered identity and marker token.
pub fn owned_roots(paths: &Paths) -> Vec<PathBuf> {
    registered(paths, true)
}

pub(crate) fn registered_roots(paths: &Paths) -> Vec<PathBuf> {
    registered(paths, false)
}

fn registered(paths: &Paths, only_verified: bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(registry(paths)) else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Registration>(&bytes).ok())
        .filter(|registration| !only_verified || verified(registration))
        .map(|registration| registration.root)
        .collect();
    roots.sort();
    roots
}

fn verified(registration: &Registration) -> bool {
    let Ok(meta) = fs::symlink_metadata(&registration.root) else {
        return false;
    };
    let Ok(marker) = fs::read(registration.root.join(MARKER)) else {
        return false;
    };
    meta.is_dir()
        && meta.dev() == registration.dev
        && meta.ino() == registration.ino
        && serde_json::from_slice::<Marker>(&marker).is_ok_and(|m| m.token == registration.token)
}

/// The project recorded when `root` was claimed.
pub(crate) fn registered_project(paths: &Paths, root: &Path) -> Option<PathBuf> {
    fs::read_dir(registry(paths))
        .ok()?
        .flatten()
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Registration>(&bytes).ok())
        .find(|registration| registration.root == root && verified(registration))
        .map(|registration| registration.project)
}

/// True when this instance's registry proves it owns `root`.
pub fn is_owned(paths: &Paths, root: &Path) -> bool {
    fs::canonicalize(root).is_ok_and(|root| owned_roots(paths).contains(&root))
}

/// Cargo's own tag, a tool database, or build profiles with fingerprints identify a build
/// cache. This says what a directory *is*, never who owns it.
pub(crate) fn looks_like_cargo_target(root: &Path) -> bool {
    if let Ok(tag) = File::open(root.join("CACHEDIR.TAG")) {
        let mut head = String::new();
        if tag
            .take(256)
            .read_to_string(&mut head)
            .is_ok_and(|_| head.contains(CACHEDIR_TAG.trim_end()))
        {
            return true;
        }
    }
    root.join(".rustc_info.json").is_file()
        || ["debug", "release"]
            .iter()
            .any(|profile| root.join(profile).join(".fingerprint").is_dir())
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug)]
pub struct Claim {
    pub root: PathBuf,
    pub already_owned: bool,
}

/// Explicit opt-in: take ownership of an existing Cargo target directory. Refuses
/// anything that is not recognizably a build cache or that holds the supervisor
/// executable, the project, or Git metadata.
pub fn adopt(paths: &Paths, project: &Path, root: &Path) -> Result<Claim> {
    let canonical =
        fs::canonicalize(root).map_err(|e| Error::Conflict(format!("{}: {e}", root.display())))?;
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(Error::Conflict(format!(
            "{} is a symbolic link; name the real directory",
            root.display()
        )));
    }
    if is_owned(paths, &canonical) {
        return Ok(Claim {
            root: canonical,
            already_owned: true,
        });
    }
    let executable = fs::canonicalize(&paths.executable).unwrap_or(paths.executable.clone());
    let project = fs::canonicalize(project).unwrap_or(project.to_path_buf());
    for (what, protected) in [
        ("the supervisor executable", &executable),
        ("the project", &project),
    ] {
        if protected.starts_with(&canonical) {
            return Err(Error::Conflict(format!(
                "{} contains {what}; it is not a build cache",
                canonical.display()
            )));
        }
    }
    if canonical.join(".git").exists() {
        return Err(Error::Conflict(format!(
            "{} holds Git metadata; it is not a build cache",
            canonical.display()
        )));
    }
    if !looks_like_cargo_target(&canonical) {
        return Err(Error::Conflict(format!(
            "{} is not recognizably a Cargo target directory",
            canonical.display()
        )));
    }
    register(paths, &project, &canonical)
}

/// Creates (or takes over an empty) cache directory for the project's own builds, which
/// proves ownership because ktask itself created it. Never touches a populated directory.
pub fn claim_fresh(paths: &Paths, project: &Path) -> Result<Option<Claim>> {
    if !project.join("Cargo.toml").is_file() {
        return Ok(None);
    }
    // Marker files must never dirty the clean Git baseline: only an ignored target qualifies.
    let ignored = std::process::Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["check-ignore", "-q", "--", "target/"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ignored {
        return Ok(None);
    }
    let target = project.join("target");
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() && fs::read_dir(&target)?.next().is_none() => {}
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&target)?,
        Err(e) => return Err(e.into()),
    }
    let canonical = fs::canonicalize(&target)?;
    let project = fs::canonicalize(project)?;
    register(paths, &project, &canonical).map(Some)
}

fn register(paths: &Paths, project: &Path, root: &Path) -> Result<Claim> {
    let token = random_token()?;
    let meta = fs::symlink_metadata(root)?;
    fs::create_dir_all(registry(paths))?;
    write_atomic(
        &root.join(MARKER),
        &serde_json::to_vec(&Marker {
            token: token.clone(),
        })
        .expect("marker JSON"),
    )?;
    if !root.join("CACHEDIR.TAG").exists() {
        fs::write(
            root.join("CACHEDIR.TAG"),
            format!("{CACHEDIR_TAG}# Cache directory tag created by ktask RS.\n"),
        )?;
    }
    let registration = Registration {
        token: token.clone(),
        root: root.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        project: project.to_path_buf(),
    };
    write_atomic(
        &registry(paths).join(format!("{token}.json")),
        &serde_json::to_vec(&registration).expect("registration JSON"),
    )?;
    // A superseded registration for the same directory no longer verifies.
    for entry in fs::read_dir(registry(paths))?.flatten() {
        if entry.file_name() != format!("{token}.json").as_str()
            && fs::read(entry.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Registration>(&b).ok())
                .is_some_and(|r| r.root == root)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(Claim {
        root: root.to_path_buf(),
        already_owned: false,
    })
}

/// Reports what a cleanup pass found and did across every owned root and the instance's
/// own scratch directories.
pub struct Cleanup {
    pub reports: Vec<Report>,
    pub scratch_reclaimed: usize,
}

/// Cleanup of everything provably owned. With `apply` false nothing is deleted.
pub fn clean(paths: &Paths, apply: bool) -> Result<Cleanup> {
    let policy = policy(paths)?;
    let mut reports = Vec::new();
    for root in owned_roots(paths) {
        reports.push(prune(paths, &root, &policy, apply)?);
    }
    let mut scratch_reclaimed = 0;
    if apply {
        for name in ["scratch", "workers"] {
            scratch_reclaimed += crate::scratch::Scratch::reclaim(&paths.cache.join(name))?;
        }
    }
    Ok(Cleanup {
        reports,
        scratch_reclaimed,
    })
}

/// Runner safe point (no worker for this project is live): claim a fresh target, then retire
/// obsolete generations in owned caches. Failures never block the queue.
pub fn maintain(paths: &Paths, project: &Path) -> Option<Cleanup> {
    if let Err(error) = claim_fresh(paths, project) {
        eprintln!("build cache: cannot claim the project target: {error}");
    }
    match clean(paths, true) {
        Ok(done) => Some(done),
        Err(error) => {
            eprintln!("build cache: cleanup failed, caches stay unbounded until fixed: {error}");
            None
        }
    }
}

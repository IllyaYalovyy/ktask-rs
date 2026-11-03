//! Generation pruning for one owned Cargo target directory.
use super::Policy;
use crate::{
    instance::Paths,
    storage::{Error, Result},
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, TryLockError},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Incremental directories differ per profile and kind (check, clippy, test, build).
const INCREMENTAL_VARIANTS: u64 = 3;

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

const LINK_NOTE: &str = "symbolic link; not managed";

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// The entries of an owned directory that cleanup may touch. A symbolic link is a boundary,
/// whether it is the directory itself or an entry inside: it is reported and never followed.
fn managed_entries(dir: &Path, skipped: &mut Vec<PathBuf>) -> Vec<fs::DirEntry> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            skipped.push(dir.to_path_buf());
            return Vec::new();
        }
        Ok(meta) if meta.is_dir() => {}
        _ => return Vec::new(),
    }
    let Ok(children) = fs::read_dir(dir) else {
        return Vec::new();
    };
    children
        .flatten()
        .filter(|child| {
            let link = child.file_type().is_ok_and(|t| t.is_symlink());
            if link {
                skipped.push(child.path());
            }
            !link
        })
        .collect()
}

fn describe(mut skipped: Vec<PathBuf>) -> Vec<String> {
    skipped.sort();
    skipped.dedup();
    skipped
        .iter()
        .map(|path| {
            let why = if is_link(path) {
                LINK_NOTE
            } else {
                "outside the owned root; not removed"
            };
            format!("{}: {why}", path.display())
        })
        .collect()
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

#[derive(PartialEq, Eq, Hash, Clone, Copy, PartialOrd, Ord)]
struct Id(u64, u64);

impl Id {
    fn of(meta: &fs::Metadata) -> Self {
        Self(meta.dev(), meta.ino())
    }
}

/// Everything that makes a file unsafe to delete right now.
struct Busy {
    inodes: HashSet<Id>,
    directories: Vec<PathBuf>,
}

fn busy_processes(root: &Path, executable: &Path) -> Busy {
    let mut busy = Busy {
        inodes: HashSet::new(),
        directories: Vec::new(),
    };
    if let Ok(meta) = fs::metadata(executable) {
        busy.inodes.insert(Id::of(&meta));
    }
    let needle = root.to_string_lossy().into_owned();
    let Ok(processes) = fs::read_dir("/proc") else {
        return busy;
    };
    for process in processes.flatten() {
        let name = process.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dir = process.path();
        if let Ok(meta) = fs::metadata(dir.join("exe")) {
            busy.inodes.insert(Id::of(&meta));
        }
        if let Ok(cwd) = fs::read_link(dir.join("cwd"))
            && cwd.starts_with(root)
        {
            busy.directories.push(cwd);
        }
        if let Ok(fds) = fs::read_dir(dir.join("fd")) {
            for fd in fds.flatten() {
                if let Ok(meta) = fs::metadata(fd.path()) {
                    busy.inodes.insert(Id::of(&meta));
                }
            }
        }
        if let Ok(maps) = fs::read_to_string(dir.join("maps")) {
            for line in maps.lines().filter(|line| line.contains(&needle)) {
                let Some(path) = line.splitn(6, ' ').nth(5).map(str::trim) else {
                    continue;
                };
                let path = path.strip_suffix(" (deleted)").unwrap_or(path);
                if let Ok(meta) = fs::metadata(path) {
                    busy.inodes.insert(Id::of(&meta));
                }
            }
        }
    }
    busy
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

fn profile_dirs(root: &Path, skipped: &mut Vec<PathBuf>) -> Vec<PathBuf> {
    let is_profile = |p: &Path| p.join("deps").is_dir() || p.join(".fingerprint").is_dir();
    // Whether a link would have been taken for a profile (or a triple holding profiles).
    let reaches_profile = |p: &Path| {
        is_profile(p)
            || fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| is_profile(&e.path()))
    };
    let mut found = Vec::new();
    let Ok(children) = fs::read_dir(root) else {
        return found;
    };
    for child in children.flatten() {
        let path = child.path();
        let Ok(kind) = child.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            if reaches_profile(&path) {
                skipped.push(path);
            }
        } else if kind.is_dir() && is_profile(&path) {
            found.push(path);
        } else if kind.is_dir()
            && let Ok(inner) = fs::read_dir(&path)
        {
            for entry in inner.flatten() {
                let inner_path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_symlink()) {
                    if is_profile(&inner_path) {
                        skipped.push(inner_path);
                    }
                } else if inner_path.is_dir() && is_profile(&inner_path) {
                    found.push(inner_path);
                }
            }
        }
    }
    found.sort();
    found
}

#[derive(Default)]
struct Totals {
    bytes: u64,
    files: u64,
}

fn allocated(meta: &fs::Metadata) -> u64 {
    let blocks = meta.blocks().saturating_mul(512);
    if meta.is_file() {
        meta.len().max(blocks)
    } else {
        blocks
    }
}

/// Allocated bytes and entries beneath (and including) `path`; hard links count once.
fn measure(path: &Path, seen: &mut HashSet<Id>) -> Totals {
    let mut totals = Totals::default();
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() && meta.nlink() > 1 && !seen.insert(Id::of(&meta)) {
            totals.files += 1;
            continue;
        }
        totals.bytes += allocated(&meta);
        totals.files += 1;
        if meta.is_dir()
            && let Ok(children) = fs::read_dir(&path)
        {
            pending.extend(children.flatten().map(|c| c.path()));
        }
    }
    totals
}

pub(super) fn measure_root(root: &Path) -> (u64, u64) {
    let totals = measure(root, &mut HashSet::new());
    (totals.bytes, totals.files)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Class {
    Executable,
    Library,
    Metadata,
}

/// One file of a generation: path, extension and metadata.
type Artifact = (PathBuf, String, fs::Metadata);

struct Generation {
    /// Crate and artifact kind; generations of one bucket are alternatives for each other.
    bucket: (String, Class, String),
    files: Vec<PathBuf>,
    extra_dirs: Vec<PathBuf>,
    newest: SystemTime,
    bytes: u64,
    entries: u64,
    local: bool,
    /// Only dependency-info files remain: nothing a build could reuse.
    remnant: bool,
    protected: bool,
    incremental: bool,
}

fn hash_suffix(stem: &str) -> Option<(&str, &str)> {
    let (base, hash) = stem.rsplit_once('-')?;
    (hash.len() == 16
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some((base, hash))
}

/// The first source a dependency-info file lists is the crate root (`src/lib.rs`,
/// `tests/tui.rs`), which tells the unit tests of a library from those of a binary.
/// Sources are relative for the project's own crates and absolute for registry crates;
/// only the former are rebuilt on every version change.
fn dep_info(path: &Path, project: &Path) -> (bool, String) {
    use std::io::Read;
    let mut text = String::new();
    if File::open(path)
        .and_then(|f| f.take(64 * 1024).read_to_string(&mut text))
        .is_err()
    {
        return (false, String::new());
    }
    let sources: Vec<&str> = text
        .lines()
        .next()
        .and_then(|line| line.split_once(": "))
        .map(|(_, sources)| sources.split_whitespace().collect())
        .unwrap_or_default();
    (
        sources.iter().any(|source| {
            !source.starts_with('/')
                || (!project.as_os_str().is_empty() && Path::new(source).starts_with(project))
        }),
        sources.first().map(|s| (*s).to_owned()).unwrap_or_default(),
    )
}

fn newest(meta: &fs::Metadata) -> SystemTime {
    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)
}

struct Context<'a> {
    busy: &'a Busy,
    /// The project whose sources this cache builds; absolute paths below it are local.
    project: &'a Path,
    finals: HashSet<Id>,
    now: SystemTime,
    grace: Duration,
}

impl Context<'_> {
    fn fresh(&self, time: SystemTime) -> bool {
        self.now.duration_since(time).unwrap_or_default() < self.grace
    }
    fn in_busy_directory(&self, path: &Path) -> bool {
        self.busy.directories.iter().any(|d| d.starts_with(path))
    }
}

/// The crate a deps/ file belongs to. Only library files carry cargo's `lib` prefix
/// (`liblibc-<h>.rlib` pairs with `libc-<h>.d`), so it is stripped from those alone.
fn crate_key(base: &str, extension: &str) -> String {
    if matches!(extension, "rlib" | "rmeta" | "so" | "a" | "dylib") {
        base.strip_prefix("lib").unwrap_or(base).to_owned()
    } else {
        base.to_owned()
    }
}

fn dependency_generations(
    profile: &Path,
    context: &Context<'_>,
    skipped: &mut Vec<PathBuf>,
) -> Vec<Generation> {
    // Within one profile a hash identifies one compilation unit, so its files, its
    // dependency-info and its fingerprint always form one generation.
    let mut by_hash: HashMap<String, Vec<Artifact>> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    {
        for file in managed_entries(&profile.join("deps"), skipped) {
            let name = file.file_name().to_string_lossy().into_owned();
            let (stem, extension) = name.split_once('.').unwrap_or((&name, ""));
            let Some((base, hash)) = hash_suffix(stem) else {
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(file.path()) else {
                continue;
            };
            // The dependency-info name is exact; library names are corrected by prefix.
            if extension == "d" || !names.contains_key(hash) {
                names.insert(hash.to_owned(), crate_key(base, extension));
            }
            by_hash.entry(hash.to_owned()).or_default().push((
                file.path(),
                extension.to_owned(),
                meta,
            ));
        }
    }
    let groups: HashMap<(String, String), Vec<Artifact>> = by_hash
        .into_iter()
        .map(|(hash, files)| ((names.remove(&hash).unwrap_or_default(), hash), files))
        .collect();
    // Fingerprint and build directories join the generation with the same hash. A
    // dependency-info-only remnant has a hash of its own, never a live generation's.
    let mut extras: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for sub in [".fingerprint", "build"] {
        for dir in managed_entries(&profile.join(sub), skipped) {
            let name = dir.file_name().to_string_lossy().into_owned();
            if let Some((_, hash)) = hash_suffix(&name) {
                extras.entry(hash.to_owned()).or_default().push(dir.path());
            }
        }
    }
    // A key is the project's own when any of its generations lists relative sources; a
    // survivor of an interrupted cleanup may have lost its own dependency-info file.
    let local_keys: HashSet<String> = groups
        .iter()
        .filter(|(_, files)| {
            files
                .iter()
                .any(|(p, e, _)| e == "d" && dep_info(p, context.project).0)
        })
        .map(|((key, _), _)| key.clone())
        .collect();
    let mut seen = HashSet::new();
    groups
        .into_iter()
        .map(|((key, hash), files)| {
            let class = if files.iter().any(|(_, e, _)| e.is_empty() || e == "exe") {
                Class::Executable
            } else if files
                .iter()
                .any(|(_, e, _)| e == "rlib" || e == "so" || e == "a")
            {
                Class::Library
            } else {
                Class::Metadata
            };
            let extra_dirs = extras.get(&hash).cloned().unwrap_or_default();
            let mut dep_infos: Vec<&PathBuf> = files
                .iter()
                .filter(|(_, e, _)| e == "d")
                .map(|(p, _, _)| p)
                .collect();
            dep_infos.sort();
            let root = dep_infos
                .first()
                .map(|p| dep_info(p, context.project).1)
                .unwrap_or_default();
            let mut generation = Generation {
                local: local_keys.contains(&key),
                bucket: (key, class, root),
                remnant: files.iter().all(|(_, e, _)| e == "d"),
                newest: files.iter().map(|(_, _, m)| newest(m)).max().unwrap(),
                bytes: 0,
                entries: 0,
                protected: false,
                incremental: false,
                files: Vec::new(),
                extra_dirs,
            };
            for (path, _, meta) in files {
                let id = Id::of(&meta);
                generation.protected |= context.busy.inodes.contains(&id)
                    || context.finals.contains(&id)
                    || context.in_busy_directory(&path);
                if !meta.is_dir() && meta.nlink() > 1 && !seen.insert(id) {
                    generation.entries += 1;
                } else {
                    generation.bytes += allocated(&meta);
                    generation.entries += 1;
                }
                generation.files.push(path);
            }
            for dir in &generation.extra_dirs.clone() {
                let totals = measure(dir, &mut seen);
                generation.bytes += totals.bytes;
                generation.entries += totals.files;
            }
            generation.protected |= context.fresh(generation.newest);
            generation
        })
        .collect()
}

fn incremental_generations(
    profile: &Path,
    context: &Context<'_>,
    skipped: &mut Vec<PathBuf>,
) -> Vec<Generation> {
    let mut out = Vec::new();
    for dir in managed_entries(&profile.join("incremental"), skipped) {
        let name = dir.file_name().to_string_lossy().into_owned();
        let Some((crate_name, hash)) = name.rsplit_once('-') else {
            continue;
        };
        if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        let path = dir.path();
        let totals = measure(&path, &mut HashSet::new());
        let mut latest = SystemTime::UNIX_EPOCH;
        let mut active = false;
        if let Ok(sessions) = fs::read_dir(&path) {
            for session in sessions.flatten() {
                if let Ok(meta) = session.metadata() {
                    latest = latest.max(newest(&meta));
                }
                if session.file_name().to_string_lossy().ends_with(".lock")
                    && let Ok(lock) = File::open(session.path())
                    && matches!(lock.try_lock_shared(), Err(TryLockError::WouldBlock))
                {
                    active = true;
                }
            }
        }
        out.push(Generation {
            bucket: (crate_name.to_owned(), Class::Metadata, String::new()),
            files: Vec::new(),
            extra_dirs: vec![path.clone()],
            newest: latest,
            bytes: totals.bytes,
            entries: totals.files,
            local: true,
            remnant: false,
            protected: active || context.fresh(latest) || context.in_busy_directory(&path),
            incremental: true,
        });
    }
    out
}

/// Marks the generations to retire: older than the warm window, and for dependencies
/// only the project's own crates (a registry crate is rebuilt only on capacity pressure).
type BucketKey<'a> = (&'a (String, Class, String), bool);

fn mark_obsolete(generations: &[Generation], keep: u64) -> HashSet<usize> {
    let mut buckets: HashMap<BucketKey<'_>, Vec<usize>> = HashMap::new();
    for (index, generation) in generations.iter().enumerate() {
        if generation.remnant {
            continue;
        }
        buckets
            .entry((&generation.bucket, generation.incremental))
            .or_default()
            .push(index);
    }
    let mut obsolete = HashSet::new();
    for (index, generation) in generations.iter().enumerate() {
        if generation.remnant && !generation.protected {
            obsolete.insert(index);
        }
    }
    for ((_, incremental), mut members) in buckets {
        members.sort_by(|a, b| generations[*b].newest.cmp(&generations[*a].newest));
        let allowed = if incremental {
            keep.saturating_mul(INCREMENTAL_VARIANTS)
        } else {
            keep
        } as usize;
        for index in members.into_iter().skip(allowed) {
            if generations[index].local && !generations[index].protected {
                obsolete.insert(index);
            }
        }
    }
    obsolete
}

/// Removal order keeps an interrupted cleanup recognizable: the files that decide a
/// generation's kind (library, then executable) go late and its dependency-info last, so a
/// survivor either still sits in its own bucket or is a lone remnant; the next pass retires it.
fn remove_generation(root: &Path, generation: &Generation, skipped: &mut Vec<PathBuf>) -> bool {
    let rank = |path: &PathBuf| match path.extension().and_then(|e| e.to_str()) {
        Some("rlib" | "so" | "a") => 1,
        None | Some("exe") => 2,
        Some("d") => 3,
        _ => 0,
    };
    let mut files: Vec<&PathBuf> = generation.files.iter().collect();
    files.sort_by_key(|path| rank(path));
    let mut complete = true;
    for dir in &generation.extra_dirs {
        if !removable(root, dir) {
            skipped.push(dir.clone());
            complete = false;
            continue;
        }
        match fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => complete = false,
        }
    }
    for file in files {
        if !removable(root, file) {
            skipped.push(file.clone());
            complete = false;
            continue;
        }
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => complete = false,
        }
    }
    complete
}

/// Retires obsolete generations of an owned target directory, then evicts the oldest
/// others while it is over `policy`. The newest generation of every artifact kind, final
/// outputs, open or mapped files, the supervisor executable and fresh output stay.
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
    let exclusive = super::lock_file(&root.join(".ktask-cache.lock"))?;
    match exclusive.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            report.deferred = Some("another cleanup is running".into());
            return Ok(report);
        }
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    let mut skipped = Vec::new();
    let profiles = profile_dirs(&root, &mut skipped);
    let _builds = match build_locks(&root, &profiles)? {
        Ok(held) => held,
        Err(reason) => {
            report.deferred = Some(reason);
            return Ok(report);
        }
    };
    (report.before_bytes, report.before_files) = measure_root(&root);
    let executable = fs::canonicalize(&paths.executable).unwrap_or(paths.executable.clone());
    let busy = busy_processes(&root, &executable);
    let project = super::registered_project(paths, &root).unwrap_or_default();
    let mut generations = Vec::new();
    for profile in &profiles {
        let mut finals = HashSet::new();
        if let Ok(files) = fs::read_dir(profile) {
            for file in files.flatten() {
                if let Ok(meta) = fs::symlink_metadata(file.path())
                    && meta.is_file()
                {
                    finals.insert(Id::of(&meta));
                }
            }
        }
        let context = Context {
            busy: &busy,
            project: &project,
            finals,
            now: SystemTime::now(),
            grace: Duration::from_secs(policy.grace_secs),
        };
        generations.extend(dependency_generations(profile, &context, &mut skipped));
        generations.extend(incremental_generations(profile, &context, &mut skipped));
    }
    report.protected_generations = generations.iter().filter(|g| g.protected).count() as u64;
    let mut doomed = mark_obsolete(&generations, policy.keep_generations);
    report.obsolete_generations = doomed.len() as u64;
    let mut bytes = report.before_bytes;
    let mut files = report.before_files;
    for &index in &doomed {
        bytes = bytes.saturating_sub(generations[index].bytes);
        files = files.saturating_sub(generations[index].entries);
    }
    if bytes > policy.max_bytes || files > policy.max_files {
        let newest_of_bucket: HashSet<usize> = {
            let mut best: HashMap<BucketKey<'_>, usize> = HashMap::new();
            for (index, generation) in generations.iter().enumerate() {
                let slot = best
                    .entry((&generation.bucket, generation.incremental))
                    .or_insert(index);
                if generation.newest > generations[*slot].newest {
                    *slot = index;
                }
            }
            best.into_values().collect()
        };
        let mut candidates: Vec<usize> = (0..generations.len())
            .filter(|i| {
                !doomed.contains(i) && !generations[*i].protected && !newest_of_bucket.contains(i)
            })
            .collect();
        candidates.sort_by_key(|i| generations[*i].newest);
        for index in candidates {
            if bytes <= policy.max_bytes && files <= policy.max_files {
                break;
            }
            bytes = bytes.saturating_sub(generations[index].bytes);
            files = files.saturating_sub(generations[index].entries);
            doomed.insert(index);
        }
        report.over_capacity = bytes > policy.max_bytes || files > policy.max_files;
    }
    if !apply {
        report.freed_bytes = doomed.iter().map(|i| generations[*i].bytes).sum();
        report.freed_files = doomed.iter().map(|i| generations[*i].entries).sum();
        report.removed_generations = doomed.len() as u64;
        report.after_bytes = bytes;
        report.after_files = files;
        report.skipped = describe(skipped);
        return Ok(report);
    }
    let mut order: Vec<usize> = doomed.into_iter().collect();
    order.sort_by_key(|i| generations[*i].newest);
    for index in order {
        if remove_generation(&root, &generations[index], &mut skipped) {
            report.removed_generations += 1;
        }
    }
    (report.after_bytes, report.after_files) = measure_root(&root);
    report.freed_bytes = report.before_bytes.saturating_sub(report.after_bytes);
    report.freed_files = report.before_files.saturating_sub(report.after_files);
    report.over_capacity =
        report.after_bytes > policy.max_bytes || report.after_files > policy.max_files;
    report.skipped = describe(skipped);
    Ok(report)
}

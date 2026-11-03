//! Reads one owned Cargo target directory into generations for the plan: what each file
//! belongs to, what it costs, and whether anything is using it. Never removes anything.
use super::plan::{Generation, Kind, Output, Totals, Unit};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, TryLockError},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// A path cleanup left alone, and why.
pub(super) type Skipped = Vec<(PathBuf, &'static str)>;

pub(super) const LINK_NOTE: &str = "symbolic link; not managed";
pub(super) const OUTSIDE_NOTE: &str = "outside the owned root; not removed";
const LAYOUT_NOTE: &str =
    "no output pairs with a Cargo fingerprint; layout not recognized, not managed";

/// The directories of a profile whose entries are generations or parts of one.
const UNIT_DIRS: [&str; 4] = ["deps", ".fingerprint", "build", "incremental"];

pub(super) fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// The entries of an owned directory that cleanup may touch. A symbolic link is a boundary,
/// whether it is the directory itself or an entry inside: it is reported and never followed.
fn managed_entries(dir: &Path, skipped: &mut Skipped) -> Vec<fs::DirEntry> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            skipped.push((dir.to_path_buf(), LINK_NOTE));
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
                skipped.push((child.path(), LINK_NOTE));
            }
            !link
        })
        .collect()
}

pub(super) fn profile_dirs(root: &Path, skipped: &mut Skipped) -> Vec<PathBuf> {
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
                skipped.push((path, LINK_NOTE));
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
                        skipped.push((inner_path, LINK_NOTE));
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

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
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

impl Busy {
    fn holds(&self, path: &Path) -> bool {
        self.directories.iter().any(|d| d.starts_with(path))
    }
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

fn allocated(meta: &fs::Metadata) -> u64 {
    let blocks = meta.blocks().saturating_mul(512);
    if meta.is_file() {
        meta.len().max(blocks)
    } else {
        blocks
    }
}

/// One walk over a tree: its total, and the total beneath every entry of a `tracked`
/// directory. Hard links count once; symbolic links are counted, never followed.
pub(super) struct Sizes {
    pub total: Totals,
    of: HashMap<PathBuf, Totals>,
}

impl Sizes {
    fn of(&self, path: &Path) -> Totals {
        self.of.get(path).copied().unwrap_or_default()
    }
}

pub(super) fn measure(root: &Path, tracked: &HashSet<PathBuf>) -> Sizes {
    let mut seen = HashSet::new();
    let mut total = Totals::default();
    let mut owners: Vec<(PathBuf, Totals)> = Vec::new();
    let mut pending: Vec<(PathBuf, Option<usize>)> = vec![(root.to_path_buf(), None)];
    while let Some((path, owner)) = pending.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let linked_again = meta.is_file() && meta.nlink() > 1 && !seen.insert(Id::of(&meta));
        let entry = Totals {
            bytes: if linked_again { 0 } else { allocated(&meta) },
            files: 1,
        };
        let owner = owner.or_else(|| {
            path.parent().filter(|p| tracked.contains(*p)).map(|_| {
                owners.push((path.clone(), Totals::default()));
                owners.len() - 1
            })
        });
        total.add(entry);
        if let Some(owner) = owner {
            owners[owner].1.add(entry);
        }
        if meta.is_dir()
            && let Ok(children) = fs::read_dir(&path)
        {
            pending.extend(children.flatten().map(|c| (c.path(), owner)));
        }
    }
    Sizes {
        total,
        of: owners.into_iter().collect(),
    }
}

fn hash_suffix(stem: &str) -> Option<(&str, &str)> {
    let (base, hash) = stem.rsplit_once('-')?;
    (hash.len() == 16
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some((base, hash))
}

/// What a dependency-info file says about its unit. Sources are relative for the project's
/// own crates and absolute for registry crates; only the former are rebuilt on every
/// version change. clippy-driver records the variables it reads as `# env-dep:CLIPPY_*`.
struct DepInfo {
    local: bool,
    clippy: bool,
}

fn dep_info(path: &Path, project: &Path) -> DepInfo {
    let mut text = String::new();
    if File::open(path)
        .and_then(|f| f.take(4 * super::MIB).read_to_string(&mut text))
        .is_err()
    {
        return DepInfo {
            local: false,
            clippy: false,
        };
    }
    let local = text
        .lines()
        .next()
        .and_then(|line| line.split_once(": "))
        .is_some_and(|(_, sources)| {
            sources.split_whitespace().any(|source| {
                !source.starts_with('/')
                    || (!project.as_os_str().is_empty() && Path::new(source).starts_with(project))
            })
        });
    let clippy = text
        .lines()
        .any(|line| line.starts_with("# env-dep:CLIPPY_"));
    DepInfo { local, clippy }
}

fn modified(meta: &fs::Metadata) -> SystemTime {
    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)
}

struct Context<'a> {
    busy: &'a Busy,
    /// The project whose sources this cache builds; absolute paths below it are local.
    project: &'a Path,
    /// Final outputs of the profile (`debug/ktask`), often hard links into `deps/`.
    finals: HashSet<Id>,
    sizes: &'a Sizes,
}

fn finals(profile: &Path) -> HashSet<Id> {
    fs::read_dir(profile)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|file| fs::symlink_metadata(file.path()).ok())
        .filter(|meta| meta.is_file())
        .map(|meta| Id::of(&meta))
        .collect()
}

/// The crate a deps/ file belongs to. Only library files carry cargo's `lib` prefix
/// (`liblibc-<h>.rlib` pairs with `libc-<h>.d`), so it is stripped from those alone.
fn crate_name(base: &str, extension: &str) -> String {
    if matches!(extension, "rlib" | "rmeta" | "so" | "a" | "dylib") {
        base.strip_prefix("lib").unwrap_or(base).to_owned()
    } else {
        base.to_owned()
    }
}

/// `.fingerprint/<package>-<hash>/` holds one unit's records, named by the unit
/// (`lib-x`, `lib-x.json`, `dep-lib-x`, `invoked.timestamp`).
struct Fingerprint {
    dir: PathBuf,
    package: String,
    unit: Option<String>,
}

fn fingerprints(profile: &Path, skipped: &mut Skipped) -> HashMap<String, Fingerprint> {
    let mut found = HashMap::new();
    for dir in managed_entries(&profile.join(".fingerprint"), skipped) {
        let name = dir.file_name().to_string_lossy().into_owned();
        let Some((package, hash)) = hash_suffix(&name) else {
            continue;
        };
        let unit = fs::read_dir(dir.path()).ok().and_then(|records| {
            records.flatten().find_map(|record| {
                let name = record.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(str::to_owned)
            })
        });
        found.insert(
            hash.to_owned(),
            Fingerprint {
                dir: dir.path(),
                package: package.to_owned(),
                unit,
            },
        );
    }
    found
}

/// One file of a generation: path, extension and metadata.
type Artifact = (PathBuf, String, fs::Metadata);

/// Within one profile a hash identifies one compilation unit, so its outputs, its
/// dependency-info, its fingerprint and its build directory form one generation.
fn unit_generations(
    profile: &Path,
    context: &Context<'_>,
    skipped: &mut Skipped,
) -> Vec<Generation> {
    let mut by_hash: HashMap<String, Vec<Artifact>> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
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
            names.insert(hash.to_owned(), crate_name(base, extension));
        }
        by_hash
            .entry(hash.to_owned())
            .or_default()
            .push((file.path(), extension.to_owned(), meta));
    }
    let mut fingerprints = fingerprints(profile, skipped);
    if !by_hash.is_empty() && !by_hash.keys().any(|hash| fingerprints.contains_key(hash)) {
        skipped.push((profile.join("deps"), LAYOUT_NOTE));
        return Vec::new();
    }
    let mut builds: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for dir in managed_entries(&profile.join("build"), skipped) {
        let name = dir.file_name().to_string_lossy().into_owned();
        if let Some((_, hash)) = hash_suffix(&name) {
            builds.entry(hash.to_owned()).or_default().push(dir.path());
        }
    }
    by_hash
        .into_iter()
        .map(|(hash, mut files)| {
            // Dependency-info goes last, after the outputs it describes.
            files.sort_by_key(|(path, extension, _)| (extension == "d", path.clone()));
            let output = if files.iter().any(|(_, e, _)| e.is_empty() || e == "exe") {
                Output::Executable
            } else if files
                .iter()
                .any(|(_, e, _)| matches!(e.as_str(), "rlib" | "so" | "a" | "dylib"))
            {
                Output::Library
            } else {
                Output::Metadata
            };
            let info = files
                .iter()
                .find(|(_, e, _)| e == "d")
                .map(|(path, _, _)| dep_info(path, context.project));
            let fingerprint = fingerprints.remove(&hash);
            let only_dep_info = files.iter().all(|(_, e, _)| e == "d");
            let kind = match &fingerprint {
                Some(Fingerprint {
                    package,
                    unit: Some(name),
                    ..
                }) if !only_dep_info => Kind::Unit(Unit {
                    package: package.clone(),
                    name: name.clone(),
                    output,
                    clippy: info.as_ref().is_some_and(|i| i.clippy),
                }),
                _ => Kind::Remnant,
            };
            // The fingerprint goes first: once it is gone, Cargo rebuilds the unit rather
            // than reuse what an interrupted cleanup left, and the next scan sees a remnant.
            let dirs = fingerprint
                .map(|f| f.dir)
                .into_iter()
                .chain(builds.remove(&hash).unwrap_or_default());
            let mut generation = Generation {
                kind,
                profile: profile.to_path_buf(),
                crate_name: names.remove(&hash).unwrap_or_default(),
                local: info.is_some_and(|i| i.local),
                in_use: files.iter().any(|(path, _, meta)| {
                    let id = Id::of(meta);
                    context.busy.inodes.contains(&id)
                        || context.finals.contains(&id)
                        || context.busy.holds(path)
                }),
                newest: files.iter().map(|(_, _, m)| modified(m)).max().unwrap(),
                size: Totals::default(),
                paths: Vec::new(),
            };
            for path in dirs.chain(files.into_iter().map(|(path, _, _)| path)) {
                let size = context.sizes.of(&path);
                generation.size.add(size);
                generation.paths.push((path, size));
            }
            generation
        })
        .collect()
}

fn incremental_generations(
    profile: &Path,
    context: &Context<'_>,
    skipped: &mut Skipped,
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
        let mut latest = SystemTime::UNIX_EPOCH;
        let mut active = false;
        if let Ok(sessions) = fs::read_dir(&path) {
            for session in sessions.flatten() {
                if let Ok(meta) = session.metadata() {
                    latest = latest.max(modified(&meta));
                }
                if session.file_name().to_string_lossy().ends_with(".lock")
                    && let Ok(lock) = File::open(session.path())
                    && matches!(lock.try_lock_shared(), Err(TryLockError::WouldBlock))
                {
                    active = true;
                }
            }
        }
        let size = context.sizes.of(&path);
        out.push(Generation {
            kind: Kind::Incremental,
            profile: profile.to_path_buf(),
            crate_name: crate_name.to_owned(),
            local: true,
            in_use: active || context.busy.holds(&path),
            newest: latest,
            size,
            paths: vec![(path, size)],
        });
    }
    out
}

pub(super) struct Scan {
    pub generations: Vec<Generation>,
    pub total: Totals,
}

/// Measures the tree once and reads every profile's generations.
pub(super) fn scan(
    root: &Path,
    profiles: &[PathBuf],
    project: &Path,
    executable: &Path,
    skipped: &mut Skipped,
) -> Scan {
    let tracked: HashSet<PathBuf> = profiles
        .iter()
        .flat_map(|profile| UNIT_DIRS.iter().map(move |dir| profile.join(dir)))
        .collect();
    let sizes = measure(root, &tracked);
    let busy = busy_processes(root, executable);
    let mut generations = Vec::new();
    for profile in profiles {
        let context = Context {
            busy: &busy,
            project,
            finals: finals(profile),
            sizes: &sizes,
        };
        generations.extend(unit_generations(profile, &context, skipped));
        generations.extend(incremental_generations(profile, &context, skipped));
    }
    Scan {
        generations,
        total: sizes.total,
    }
}

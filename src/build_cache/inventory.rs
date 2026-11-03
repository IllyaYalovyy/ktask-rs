//! Read-only inventory: what exists, who owns it, and what the bound does not cover.
use super::{Policy, scan};
use crate::instance::Paths;
use serde::Serialize;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Build,
    Scratch,
    Diagnostic,
    Provider,
    External,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Scratch => "scratch",
            Self::Diagnostic => "diagnostic",
            Self::Provider => "provider",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    /// Verified marker and registration: cleanup manages it.
    Owned,
    /// Inventoried only, until the owner opts in.
    Unowned,
    /// Never cleaned.
    Excluded,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: Kind,
    pub ownership: Ownership,
    pub bytes: u64,
    pub files: u64,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub policy: Policy,
    pub owned: Vec<Entry>,
    pub unowned: Vec<Entry>,
    pub provider: Vec<Entry>,
    pub managed_bytes: u64,
    pub managed_files: u64,
    pub unowned_bytes: u64,
    pub provider_bytes: u64,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub pressure: Vec<String>,
    pub exclusions: Vec<String>,
}

/// Human-readable allocated size.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl Inventory {
    /// The one text form of an inventory, shown by the TUI and printed by the CLI.
    pub fn render(&self) -> String {
        let mut text = format!(
            "Managed: {} in {} files (limit {} / {} files per cache) · free {} of {}\n",
            size(self.managed_bytes),
            self.managed_files,
            size(self.policy.max_bytes),
            self.policy.max_files,
            size(self.free_bytes),
            size(self.total_bytes),
        );
        for (title, entries) in [
            ("Owned", &self.owned),
            ("Not managed (opt in to clean)", &self.unowned),
            ("Provider storage (never cleaned)", &self.provider),
        ] {
            if entries.is_empty() {
                continue;
            }
            text.push_str(&format!("{title}:\n"));
            for e in entries {
                text.push_str(&format!(
                    "  {} · {} · {} · {} files · {}\n",
                    e.path.display(),
                    e.kind.as_str(),
                    size(e.bytes),
                    e.files,
                    e.note
                ));
            }
        }
        for line in &self.pressure {
            text.push_str(&format!("Pressure: {line}\n"));
        }
        text.push_str(&format!("Never touched: {}\n", self.exclusions.join("; ")));
        text
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn home(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(fallback))
        })
}

fn provider_roots() -> Vec<(&'static str, PathBuf)> {
    let mut roots = Vec::new();
    let mut add = |name: &'static str, path: Option<PathBuf>| {
        if let Some(path) = path.filter(|p| exists(p)) {
            roots.push((name, path));
        }
    };
    add("Codex", home("CODEX_HOME", ".codex"));
    add("Claude Code", home("CLAUDE_CONFIG_DIR", ".claude"));
    let data = home("XDG_DATA_HOME", ".local/share");
    let state = home("XDG_STATE_HOME", ".local/state");
    add("Claude Code data", data.as_ref().map(|d| d.join("claude")));
    add("OpenCode", data.as_ref().map(|d| d.join("opencode")));
    add("OpenCode state", state.as_ref().map(|d| d.join("opencode")));
    roots
}

fn filesystem(path: &Path) -> (u64, u64) {
    let mut probe = path;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return (0, 0),
        }
    }
    nix::sys::statvfs::statvfs(probe).map_or((0, 0), |s| {
        let unit = s.fragment_size();
        (s.blocks_available() * unit, s.blocks() * unit)
    })
}

/// What [`discover`] found, before anything is measured.
#[derive(Default)]
struct Found {
    owned: Vec<Entry>,
    unowned: Vec<Entry>,
    provider: Vec<Entry>,
    exclusions: Vec<String>,
    /// Canonical paths already listed, so no directory appears twice.
    claimed: Vec<PathBuf>,
}

impl Found {
    fn add(&mut self, path: &Path, kind: Kind, ownership: Ownership, note: &str) {
        let entry = Entry {
            path: path.to_path_buf(),
            kind,
            ownership,
            bytes: 0,
            files: 0,
            note: note.into(),
        };
        match (kind, ownership) {
            (Kind::Provider, _) => self.provider.push(entry),
            (_, Ownership::Owned) => self.owned.push(entry),
            _ => self.unowned.push(entry),
        }
        self.claimed
            .push(fs::canonicalize(path).unwrap_or(path.to_path_buf()));
    }
    fn listed(&self, path: &Path) -> bool {
        self.claimed
            .contains(&fs::canonicalize(path).unwrap_or(path.to_path_buf()))
    }
}

fn discover_builds(found: &mut Found, paths: &Paths, project: &Path) {
    for root in super::owned_roots(paths) {
        found.add(
            &root,
            Kind::Build,
            Ownership::Owned,
            "verified marker and registration; obsolete generations retire automatically",
        );
    }
    for root in super::registered_roots(paths) {
        if !found.listed(&root) && exists(&root) {
            found.add(
                &root,
                Kind::Build,
                Ownership::Unowned,
                "registration no longer verifies (moved, copied or marker removed); adopt again to manage",
            );
        }
    }
    let target = project.join("target");
    if exists(&target) && !found.listed(&target) {
        let note = if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            "symbolic link; never followed. Name the real directory to adopt it"
        } else if super::looks_like_cargo_target(&target) {
            "project build cache created outside ktask; run `cache adopt` to manage it"
        } else {
            "project directory named target; not recognizably a build cache"
        };
        found.add(&target, Kind::Build, Ownership::Unowned, note);
    }
}

fn discover_instance_cache(found: &mut Found, paths: &Paths) {
    let Ok(children) = fs::read_dir(&paths.cache) else {
        return;
    };
    let mut children: Vec<_> = children.flatten().map(|c| c.path()).collect();
    children.sort();
    for path in children {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == super::REGISTRY || found.listed(&path) || !path.is_dir() {
            continue;
        }
        if name == "scratch" || name == "workers" {
            found.add(
                &path,
                Kind::Scratch,
                Ownership::Owned,
                "per-run directories under kernel leases; abandoned runs are reclaimed at startup and safe points",
            );
        } else if super::looks_like_cargo_target(&path) {
            found.add(
                &path,
                Kind::Build,
                Ownership::Unowned,
                "build cache inside the instance cache directory with no ownership proof",
            );
        } else {
            found.add(
                &path,
                Kind::External,
                Ownership::Unowned,
                "unrecognized; left alone",
            );
        }
    }
}

fn discover_elsewhere(found: &mut Found, paths: &Paths) {
    let temp = std::env::temp_dir();
    for name in ["ktask-diag", "ktask-test-scratch"] {
        let path = temp.join(name);
        if exists(&path) {
            found.add(
                &path,
                Kind::Diagnostic,
                Ownership::Unowned,
                "diagnostic tree from earlier tooling; owner never proven",
            );
        }
    }
    if let Some(siblings) = paths.cache.parent().and_then(|p| fs::read_dir(p).ok()) {
        let mut names: Vec<PathBuf> = siblings
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p != &paths.cache
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("ktask"))
                    && p.is_dir()
            })
            .collect();
        names.sort();
        for path in names {
            found.add(
                &path,
                Kind::External,
                Ownership::Excluded,
                "another instance's or legacy cache; not this instance's data",
            );
        }
    }
    for (name, path) in provider_roots() {
        found.add(
            &path,
            Kind::Provider,
            Ownership::Excluded,
            &format!(
                "{name}: resumable sessions and credentials preserved; no supported scoped cleanup"
            ),
        );
    }
}

/// Every directory the instance knows about, with its owner and what cleanup does with it.
fn discover(paths: &Paths, project: &Path) -> Found {
    let mut found = Found::default();
    discover_builds(&mut found, paths, project);
    discover_instance_cache(&mut found, paths);
    discover_elsewhere(&mut found, paths);
    found.exclusions = vec![
        "Project sources, Git objects, stashes and worktrees".to_owned(),
        format!("Installed supervisor {}", paths.executable.display()),
        "Evidence, transcripts and the SQLite database (see `storage`)".to_owned(),
        "Provider session, credential and cache storage (reported separately; never cleaned)"
            .to_owned(),
        "Other applications' data".to_owned(),
    ];
    found
}

/// Allocated bytes and entries of each entry's tree.
fn measure(entries: &mut [Entry]) {
    for entry in entries {
        let totals = scan::measure(&entry.path, &HashSet::new()).total;
        (entry.bytes, entry.files) = (totals.bytes, totals.files);
    }
}

/// Totals and the pressure the bound does or cannot relieve.
fn report(policy: Policy, found: Found, filesystem: (u64, u64)) -> Inventory {
    let (free_bytes, total_bytes) = filesystem;
    let managed_bytes = found.owned.iter().map(|e| e.bytes).sum();
    let managed_files = found.owned.iter().map(|e| e.files).sum();
    let unowned_bytes = found
        .unowned
        .iter()
        .filter(|e| e.ownership == Ownership::Unowned)
        .map(|e| e.bytes)
        .sum();
    let provider_bytes = found.provider.iter().map(|e| e.bytes).sum();
    let mut pressure = Vec::new();
    for entry in found.owned.iter().filter(|e| e.kind == Kind::Build) {
        if entry.bytes > policy.max_bytes || entry.files > policy.max_files {
            pressure.push(format!(
                "{} exceeds its limit ({} of {} bytes, {} of {} files); cleanup retires obsolete generations",
                entry.path.display(), entry.bytes, policy.max_bytes, entry.files, policy.max_files
            ));
        }
    }
    if total_bytes > 0 && free_bytes < policy.min_free_bytes {
        pressure.push(format!(
            "cache filesystem has {free_bytes} free bytes, below the {} byte floor",
            policy.min_free_bytes
        ));
    }
    if unowned_bytes > 0 {
        pressure.push(format!(
            "{unowned_bytes} bytes in unowned caches are outside the bound until adopted"
        ));
    }
    if provider_bytes > 0 {
        pressure.push(format!(
            "{provider_bytes} bytes of provider storage grow outside the bound"
        ));
    }
    Inventory {
        policy,
        owned: found.owned,
        unowned: found.unowned,
        provider: found.provider,
        managed_bytes,
        managed_files,
        unowned_bytes,
        provider_bytes,
        free_bytes,
        total_bytes,
        pressure,
        exclusions: found.exclusions,
    }
}

pub fn inventory(paths: &Paths, project: &Path) -> crate::storage::Result<Inventory> {
    let policy = super::policy(paths)?;
    let mut found = discover(paths, project);
    for entries in [&mut found.owned, &mut found.unowned, &mut found.provider] {
        measure(entries);
    }
    Ok(report(policy, found, filesystem(&paths.cache)))
}

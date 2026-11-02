//! Read-only inventory: what exists, who owns it, and what the bound does not cover.
use super::{Policy, prune::measure_root};
use crate::instance::Paths;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    /// build, scratch, diagnostic, provider or external.
    pub kind: &'static str,
    /// owned (verified), unowned (inventoried only) or excluded (never cleaned).
    pub ownership: &'static str,
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

fn entry(path: &Path, kind: &'static str, ownership: &'static str, note: &str) -> Entry {
    let (bytes, files) = measure_root(path);
    Entry {
        path: path.to_path_buf(),
        kind,
        ownership,
        bytes,
        files,
        note: note.into(),
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

pub fn inventory(paths: &Paths, project: &Path) -> crate::storage::Result<Inventory> {
    let policy = super::policy(paths)?;
    let mut owned = Vec::new();
    let mut unowned = Vec::new();
    let mut claimed = Vec::new();
    for root in super::owned_roots(paths) {
        owned.push(entry(
            &root,
            "build",
            "owned",
            "verified marker and registration; obsolete generations retire automatically",
        ));
        claimed.push(root);
    }
    for root in super::registered_roots(paths) {
        if !claimed.contains(&root) && exists(&root) {
            unowned.push(entry(
                &root,
                "build",
                "unowned",
                "registration no longer verifies (moved, copied or marker removed); adopt again to manage",
            ));
            claimed.push(root);
        }
    }
    let target = project.join("target");
    if exists(&target) {
        let canonical = fs::canonicalize(&target).unwrap_or(target.clone());
        if !claimed.contains(&canonical) {
            let note = if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
                "symbolic link; never followed. Name the real directory to adopt it"
            } else if super::looks_like_cargo_target(&target) {
                "project build cache created outside ktask; run `cache adopt` to manage it"
            } else {
                "project directory named target; not recognizably a build cache"
            };
            unowned.push(entry(&target, "build", "unowned", note));
            claimed.push(canonical);
        }
    }
    if let Ok(children) = fs::read_dir(&paths.cache) {
        let mut children: Vec<_> = children.flatten().map(|c| c.path()).collect();
        children.sort();
        for path in children {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let canonical = fs::canonicalize(&path).unwrap_or(path.clone());
            if name == super::REGISTRY || claimed.contains(&canonical) || !path.is_dir() {
                continue;
            }
            if name == "scratch" || name == "workers" {
                owned.push(entry(
                    &path,
                    "scratch",
                    "owned",
                    "per-run directories under kernel leases; abandoned runs are reclaimed at startup and safe points",
                ));
            } else if super::looks_like_cargo_target(&path) {
                unowned.push(entry(
                    &path,
                    "build",
                    "unowned",
                    "build cache inside the instance cache directory with no ownership proof",
                ));
            } else {
                unowned.push(entry(
                    &path,
                    "external",
                    "unowned",
                    "unrecognized; left alone",
                ));
            }
        }
    }
    let temp = std::env::temp_dir();
    for name in ["ktask-diag", "ktask-test-scratch"] {
        let path = temp.join(name);
        if exists(&path) {
            unowned.push(entry(
                &path,
                "diagnostic",
                "unowned",
                "diagnostic tree from earlier tooling; owner never proven",
            ));
        }
    }
    let mut exclusions = vec![
        "Project sources, Git objects, stashes and worktrees".to_owned(),
        format!("Installed supervisor {}", paths.executable.display()),
        "Evidence, transcripts and the SQLite database (see `storage`)".to_owned(),
        "Provider session, credential and cache storage (reported separately; never cleaned)"
            .to_owned(),
    ];
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
            unowned.push(entry(
                &path,
                "external",
                "excluded",
                "another instance's or legacy cache; not this instance's data",
            ));
        }
    }
    let provider: Vec<Entry> = provider_roots()
        .into_iter()
        .map(|(name, path)| {
            entry(
                &path,
                "provider",
                "excluded",
                &format!("{name}: resumable sessions and credentials preserved; no supported scoped cleanup"),
            )
        })
        .collect();
    exclusions.push("Other applications' data".into());
    let managed_bytes = owned.iter().map(|e| e.bytes).sum();
    let managed_files = owned.iter().map(|e| e.files).sum();
    let unowned_bytes = unowned
        .iter()
        .filter(|e| e.ownership == "unowned")
        .map(|e| e.bytes)
        .sum();
    let provider_bytes = provider.iter().map(|e| e.bytes).sum();
    let (free_bytes, total_bytes) = filesystem(&paths.cache);
    let mut pressure = Vec::new();
    for entry in owned.iter().filter(|e| e.kind == "build") {
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
    Ok(Inventory {
        policy,
        owned,
        unowned,
        provider,
        managed_bytes,
        managed_files,
        unowned_bytes,
        provider_bytes,
        free_bytes,
        total_bytes,
        pressure,
        exclusions,
    })
}

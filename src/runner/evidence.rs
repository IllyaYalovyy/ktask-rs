//! Worker evidence handoff under the workspace worker lease.
use super::{Workspace, process};
use crate::{
    application::{Service, collection::Collection},
    storage::{Database, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const COLLECTION_PREFIX: &str = "collection-";
const CLEANUP_PREFIX: &str = "cleanup-";

#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactTarget {
    pub state: PathBuf,
    pub project_id: i64,
    pub project: PathBuf,
    pub scratch_root: PathBuf,
    pub attempt: i64,
    pub token: String,
    pub relative_prefix: String,
    /// Record the collection even when the worker kept nothing, so that a reader can tell
    /// that from evidence that was never collected.
    pub record_empty: bool,
}

/// The files and this manifest survive the removal of managed worker scratch. The
/// next runner imports them through the same collection path as an interrupted archive.
#[derive(Serialize, Deserialize)]
struct Deferred {
    attempt: i64,
    token: String,
    prefix: String,
    tree: String,
    note: String,
}

pub(crate) fn deferred_root(workers: &Path, project: i64) -> PathBuf {
    workers.join(format!("evidence-project-{project}"))
}

impl Deferred {
    fn keep(source: &Path, target: &ArtifactTarget, tree: String, note: String) -> Result<()> {
        let root = deferred_root(&target.scratch_root, target.project_id);
        fs::create_dir_all(&root)?;
        let handoff = tempfile::Builder::new()
            .prefix(COLLECTION_PREFIX)
            .tempdir_in(
                source
                    .parent()
                    .expect("artifact directory has a scratch parent"),
            )?;
        let manifest = Self {
            attempt: target.attempt,
            token: target.token.clone(),
            prefix: target.relative_prefix.clone(),
            tree,
            note,
        };
        let file = fs::File::create(handoff.path().join("manifest.json"))?;
        serde_json::to_writer(&file, &manifest).map_err(io::Error::other)?;
        file.sync_all()?;
        fs::rename(source, handoff.path().join("files"))?;
        let name = handoff.path().file_name().expect("handoff directory name");
        fs::rename(handoff.path(), root.join(name))?;
        Ok(())
    }

    fn import(path: &Path, service: &Service) -> Result<()> {
        let manifest: Self = serde_json::from_reader(fs::File::open(path.join("manifest.json"))?)
            .map_err(io::Error::other)?;
        let source = path.join("files");
        let collection = Collection {
            attempt: manifest.attempt,
            token: &manifest.token,
            prefix: &manifest.prefix,
            tree: &manifest.tree,
            source: &source,
        };
        service.import_deferred_collection(&collection, &manifest.note)
    }
}

/// The guardian owns the worker lease while publishing or handing off evidence.
pub(super) fn archive_artifacts(source: &Path, target: &ArtifactTarget) -> Option<String> {
    if !fs::symlink_metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        return Some(
            "Evidence could not be archived: the artifact directory was replaced or removed".into(),
        );
    }
    if !target.record_empty
        && fs::read_dir(source).is_ok_and(|mut entries| entries.next().is_none())
    {
        return None;
    }
    let tree = match Workspace::fingerprint_at(&target.project, &target.scratch_root) {
        Ok(tree) => tree,
        Err(error) => return Some(format!("Evidence could not be archived: {error}")),
    };
    let kept = target.scratch_root.join(format!(
        "evidence-{}-{}",
        target.attempt,
        std::process::id()
    ));
    let collection = |source| Collection {
        attempt: target.attempt,
        token: &target.token,
        prefix: &target.relative_prefix,
        source,
        tree: &tree,
    };
    let reserved = (|| -> Result<Service> {
        let service = Service::open(Database::open_in(&target.state)?, &target.project, None)?;
        service.reserve_collection(&collection(&kept))?;
        Ok(service)
    })();
    let service = match reserved {
        Ok(service) => service,
        Err(error) => {
            let note = format!(
                "Evidence archiving deferred for {}: {error}. The runner will recover the retained collection.",
                target.relative_prefix
            );
            return Some(match Deferred::keep(source, target, tree, note.clone()) {
                Ok(()) => note,
                Err(retention) => format!(
                    "Evidence could not be archived: {error}. Retaining the files failed: {retention}"
                ),
            });
        }
    };
    let source = match fs::rename(source, &kept) {
        Ok(()) => kept.as_path(),
        Err(_) => source,
    };
    service
        .collect_evidence(&collection(source))
        .unwrap_or_else(|error| {
            Some(format!(
                "Evidence not archived yet: {error}. The runner will finish or record the collection when it continues"
            ))
        })
}

/// The worker lease excludes a live guardian and serializes handoff for this project.
pub(super) fn finish(service: &Service, workspace: &Workspace, workers: &Path) -> Result<()> {
    let Ok(_lease) = process::lock(&workspace.worker_lock) else {
        return Ok(());
    };
    let root = deferred_root(workers, service.project_id);
    let mut handoffs = Vec::new();
    match fs::read_dir(&root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let path = entry.path();
                if let Some(id) = name.strip_prefix(COLLECTION_PREFIX) {
                    Deferred::import(&path, service)?;
                    handoffs.push((path, root.join(format!("{CLEANUP_PREFIX}{id}"))));
                } else if name.starts_with(CLEANUP_PREFIX) {
                    remove_handoff(&path);
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    for note in service.finish_pending_evidence()? {
        note!("{note}");
    }
    for (path, cleanup) in handoffs {
        // Keep the manifest available for replay until the collection settles, then
        // retire the whole directory before deleting any of it.
        match fs::rename(&path, &cleanup) {
            Ok(()) => remove_handoff(&cleanup),
            Err(error) => note!(
                "Cannot set aside recovered evidence staging at {} ({error})",
                path.display()
            ),
        }
    }
    if let Err(error) = fs::remove_dir(&root)
        && !matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
        )
    {
        note!(
            "Cannot remove deferred evidence directory {}: {error}",
            root.display()
        );
    }
    Ok(())
}

fn remove_handoff(path: &Path) {
    if let Err(error) = crate::scratch::remove_tree(path) {
        note!(
            "Cannot remove recovered evidence staging at {} ({error})",
            path.display()
        );
    }
}

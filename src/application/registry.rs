//! Managing a registered project from outside it: pointing it at another folder, and
//! deleting it with everything ktask stored for it. Neither touches the project's folder.
use super::{require, retention};
use crate::{
    instance::Paths,
    runner::{investigate, registration},
    storage::{Database, Error, Result},
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Rows that belong to an attempt and carry no project of their own.
const BY_ATTEMPT: [&str; 13] = [
    "resource_breaches",
    "attempt_delivery",
    "attempt_progress",
    "lens_review_findings",
    "lens_reviews",
    "questions",
    // Before its usage counters: deleting output updates them.
    "attempt_output",
    "attempt_output_usage",
    "output_pruning",
    "output_pruning_progress",
    "evidence_cleanup_admissions",
    "artifacts",
    "artifact_reservations",
];
/// Rows that belong to a task and carry no project of their own.
const BY_TASK: [&str; 7] = [
    "criterion_results",
    "acceptance_criteria",
    "revision_lenses",
    "revision_categories",
    "revision_models",
    "task_revisions",
    "task_lifecycle_events",
];
/// Rows that belong to an investigation and carry no project of their own.
const BY_INVESTIGATION: [&str; 3] = [
    "investigation_progress",
    "investigation_references",
    "investigation_fix_steps",
];
const BY_PROJECT: [&str; 27] = [
    "investigations",
    "project_investigation_limits",
    "pending_controls",
    "events",
    "model_calls",
    "attempt_steps",
    "ai_sessions",
    "task_requirements",
    "task_links",
    "attempts",
    "tasks",
    "project_checks",
    "project_lens_defaults",
    "review_lenses",
    "work_categories",
    "project_models",
    "execution_sessions",
    "project_delivery",
    "provision_consents",
    "provision_policy",
    "project_tool_epochs",
    "project_tools",
    "project_retention",
    "project_storage_limits",
    "instance_runner_log_baselines",
    "evidence_sweep",
    "evidence_pins",
];

/// What deleting a project removed, and what it could not.
#[derive(Debug)]
pub struct Removed {
    pub id: i64,
    pub name: String,
    pub root: PathBuf,
    pub tasks: i64,
    pub attempts: i64,
    /// ktask files that could not be removed; the project's rows are gone regardless.
    pub leftovers: Vec<String>,
}

fn named(db: &Database, id: i64) -> Result<(String, PathBuf)> {
    db.connection()
        .query_row(
            "SELECT name, root_path FROM projects WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .map(|(name, root)| {
            use std::os::unix::ffi::OsStringExt;
            (name, PathBuf::from(std::ffi::OsString::from_vec(root)))
        })
        .ok_or_else(|| Error::Conflict(format!("Project {id} is not registered")))
}

/// Holds the project's runner, investigation and provisioning locks, so nothing starts
/// meanwhile.
struct Idle {
    _runner: registration::Claim,
    _investigation: fs::File,
    _provision: Option<fs::File>,
}

fn idle(db: &Database, paths: &Paths, id: i64, name: &str) -> Result<Idle> {
    let runner = registration::claim(paths, id)?.ok_or_else(|| {
        Error::Conflict(format!(
            "{name} has a live runner · pause it first (ktask pause)"
        ))
    })?;
    let investigation = investigate::claim(paths, id)?.ok_or_else(|| {
        Error::Conflict(format!(
            "An agent report is being written for {name} · stop it first (ktask agent stop ID)"
        ))
    })?;
    let provision = match fs::File::open(provision_file(paths, id, "lock")) {
        Ok(lock) => {
            require(
                lock.try_lock().is_ok(),
                "This project is provisioning tools · wait until it finishes",
            )?;
            Some(lock)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let pinned: bool = db.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM evidence_pins WHERE project_id = ?1)",
        [id],
        |r| r.get(0),
    )?;
    require(
        !pinned,
        "An interactive AI session is still using this project · close it first",
    )?;
    Ok(Idle {
        _runner: runner,
        _investigation: investigation,
        _provision: provision,
    })
}

fn provision_file(paths: &Paths, id: i64, extension: &str) -> PathBuf {
    paths
        .state
        .join("provision")
        .join(format!("{id}.{extension}"))
}

/// Point a project at another existing folder. ktask moves nothing: the owner moves or
/// clones the repository, and the project's tasks and history follow it here.
pub fn relocate(db: &mut Database, paths: &Paths, id: i64, folder: &Path) -> Result<PathBuf> {
    let (name, old) = named(db, id)?;
    require(
        folder.is_absolute(),
        "The new folder must be an absolute path",
    )?;
    let root = folder
        .canonicalize()
        .map_err(|error| Error::Conflict(format!("Cannot use {}: {error}", folder.display())))?;
    require(root.is_dir(), "The new folder must be a directory")?;
    require(root != old, "The project is already in that folder")?;
    let _idle = idle(db, paths, id, &name)?;
    let bytes = root.as_os_str().as_encoded_bytes();
    let tx = db
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let owner: Option<String> = tx
        .query_row(
            "SELECT name FROM projects WHERE root_path = ?1",
            [bytes],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(owner) = owner {
        return Err(Error::Conflict(format!(
            "That folder is already project {owner}"
        )));
    }
    let moved = tx.execute(
        "UPDATE projects SET root_path = ?1 WHERE id = ?2",
        rusqlite::params![bytes, id],
    )?;
    require(moved == 1, "That project is no longer registered")?;
    tx.commit()?;
    Ok(root)
}

/// Delete a project: every row it owns, then every file ktask keeps for it. `confirm`
/// must be its exact name. The rows go in one transaction; files that then cannot be
/// removed are reported, never a reason to keep half a project.
pub fn remove(db: &mut Database, paths: &Paths, id: i64, confirm: &str) -> Result<Removed> {
    let (name, root) = named(db, id)?;
    require(
        confirm == name,
        "Type the project's exact name to delete it",
    )?;
    let idle = idle(db, paths, id, &name)?;
    let tx = db
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    // A reference this list misses fails the commit and keeps the whole project.
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    let attempts = tx
        .prepare("SELECT id FROM attempts WHERE project_id = ?1")?
        .query_map([id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let tasks: i64 = tx.query_row(
        "SELECT count(*) FROM tasks WHERE project_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    tx.execute(
        "DELETE FROM check_timeouts WHERE step_id IN (SELECT id FROM attempt_steps WHERE project_id = ?1)",
        [id],
    )?;
    for table in BY_ATTEMPT {
        tx.execute(
            &format!("DELETE FROM {table} WHERE attempt_id IN (SELECT id FROM attempts WHERE project_id = ?1)"),
            [id],
        )?;
    }
    for table in BY_TASK {
        tx.execute(
            &format!(
                "DELETE FROM {table} WHERE task_id IN (SELECT id FROM tasks WHERE project_id = ?1)"
            ),
            [id],
        )?;
    }
    for table in BY_INVESTIGATION {
        tx.execute(
            &format!("DELETE FROM {table} WHERE investigation_id IN (SELECT id FROM investigations WHERE project_id = ?1)"),
            [id],
        )?;
    }
    for table in BY_PROJECT {
        tx.execute(&format!("DELETE FROM {table} WHERE project_id = ?1"), [id])?;
    }
    tx.execute("DELETE FROM projects WHERE id = ?1", [id])?;
    retention::bump_admission_generation(&tx)?;
    tx.commit().map_err(|error| {
        Error::Conflict(format!(
            "{name} was not deleted; something still refers to it: {error}"
        ))
    })?;
    let leftovers = remove_files(paths, id, &attempts);
    drop(idle);
    let lease = registration::lease_file(paths, id);
    let mut leftovers = leftovers;
    discard(&lease, &mut leftovers);
    Ok(Removed {
        id,
        name,
        root,
        tasks,
        attempts: attempts.len() as i64,
        leftovers,
    })
}

fn discard(path: &Path, leftovers: &mut Vec<String>) {
    let removed = match fs::symlink_metadata(path) {
        Ok(found) if found.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => leftovers.push(format!("{}: {error}", path.display())),
    }
}

/// Everything under the instance roots that is named after the project or its attempts.
fn remove_files(paths: &Paths, id: i64, attempts: &[i64]) -> Vec<String> {
    let mut leftovers = Vec::new();
    for attempt in attempts {
        discard(
            &paths.state.join(format!("evidence/attempt-{attempt}")),
            &mut leftovers,
        );
    }
    // The runner log and its rotated segments.
    let log = format!("runner-{id}.log");
    match fs::read_dir(&paths.state) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let file = entry.file_name();
                let file = file.to_string_lossy();
                if file == log
                    || file
                        .strip_prefix(&log)
                        .is_some_and(|rest| rest.starts_with('.'))
                {
                    discard(&entry.path(), &mut leftovers);
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => leftovers.push(format!("{}: {error}", paths.state.display())),
    }
    for path in [
        provision_file(paths, id, "json"),
        provision_file(paths, id, "lock"),
        paths.config.join(format!("project-{id}.json")),
        paths.cache.join("project-runtime").join(id.to_string()),
    ]
    .into_iter()
    .chain(investigate::files(paths, id))
    {
        discard(&path, &mut leftovers);
    }
    leftovers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Definition, Kind, ModelConfig, Service, VersionedTask};
    use std::collections::BTreeMap;

    /// Rows every project shares, or that count the instance rather than a project.
    const SHARED: [&str; 4] = [
        "category_snapshots",
        "instance_storage_admission",
        "instance_allocation_scan",
        "sqlite_sequence",
    ];

    struct Fixture {
        paths: Paths,
        _sandbox: crate::scratch::Scratch,
    }

    fn fixture() -> Result<Fixture> {
        let sandbox =
            crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let at = |name: &str| sandbox.path().join(name);
        let paths = Paths {
            config: at("config"),
            state: at("state"),
            cache: at("cache"),
            data: at("data"),
            runtime: at("runtime"),
            executable: at("ktask"),
        };
        Ok(Fixture {
            paths,
            _sandbox: sandbox,
        })
    }

    /// A project with a finished task, a failed attempt with a step and output, and
    /// every kind of file ktask keeps for a project. Returns its id and attempt.
    fn populated(f: &Fixture, name: &str) -> Result<(i64, i64)> {
        let folder = f.paths.state.parent().unwrap().join(name);
        fs::create_dir_all(&folder)?;
        fs::write(folder.join("README"), name)?;
        let mut service = Service::open(Database::open_in(&f.paths.state)?, &folder, None)?;
        let definition = |title: &str| Definition {
            title: title.into(),
            goal: "Be deleted with the project".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Done".into()],
            models: std::array::from_fn(|_| {
                Some(ModelConfig {
                    provider: "claude".into(),
                    model: "sonnet".into(),
                    effort: None,
                })
            }),
            attempt_limit: Some(2),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        let ids = service.create(&[definition("First"), definition("Second")], None, true)?;
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask {
                id: ids[0],
                version,
            },
            &format!("{name}-run"),
            "fixture-base",
            false,
        )?;
        let db = service.db.connection();
        db.execute(
            "INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) SELECT id,project_id,task_id,revision,1,'check','succeeded','true','.',1000,1,2 FROM attempts WHERE id=?1",
            [run.attempt],
        )?;
        service.finish_run(&run, "failed", Some("check"), "Failed", None)?;
        let id = service.project_id;
        let evidence = f
            .paths
            .state
            .join(format!("evidence/attempt-{}", run.attempt));
        fs::create_dir_all(evidence.join("step-1"))?;
        fs::write(evidence.join("step-1/file"), "evidence")?;
        for file in [
            f.paths.state.join(format!("runner-{id}.log")),
            f.paths.state.join(format!("runner-{id}.log.1")),
            f.paths.state.join(format!("runners/{id}.lock")),
            f.paths.state.join(format!("provision/{id}.json")),
            f.paths.state.join(format!("provision/{id}.lock")),
            f.paths.config.join(format!("project-{id}.json")),
            f.paths.cache.join(format!("project-runtime/{id}/bin/tool")),
        ] {
            fs::create_dir_all(file.parent().unwrap())?;
            fs::write(file, "ktask")?;
        }
        Ok((id, run.attempt))
    }

    fn row_counts(db: &Database) -> Result<BTreeMap<String, i64>> {
        let db = db.connection();
        let tables = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tables
            .into_iter()
            .filter(|table| !SHARED.contains(&table.as_str()))
            .map(|table| {
                let count =
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
                Ok((table, count))
            })
            .collect()
    }

    fn files(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(entry.path());
                } else {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn deleting_a_project_removes_only_its_rows_and_ktask_files() -> Result<()> {
        let f = fixture()?;
        let (kept, kept_attempt) = populated(&f, "kept")?;
        let before = row_counts(&Database::open_in(&f.paths.state)?)?;
        let ktask_roots = [&f.paths.config, &f.paths.cache];
        let kept_files: Vec<_> = ktask_roots.iter().flat_map(|root| files(root)).collect();
        let (doomed, doomed_attempt) = populated(&f, "doomed")?;
        let folder = f.paths.state.parent().unwrap().join("doomed");
        let project_files = files(&folder);

        let mut db = Database::open_in(&f.paths.state)?;
        assert!(row_counts(&db)?["attempt_steps"] > before["attempt_steps"]);
        let wrong = remove(&mut db, &f.paths, doomed, "Doomed").unwrap_err();
        assert!(wrong.to_string().contains("exact name"), "{wrong}");
        let removed = remove(&mut db, &f.paths, doomed, "doomed")?;
        assert_eq!((removed.tasks, removed.attempts), (2, 1));
        assert_eq!(removed.leftovers, Vec::<String>::new());
        assert_eq!(removed.root, folder.canonicalize()?);

        // Every table holds exactly what it held before the deleted project existed.
        assert_eq!(row_counts(&db)?, before);
        assert!(
            !db.connection()
                .prepare("PRAGMA foreign_key_check")?
                .exists([])?
        );
        assert_eq!(
            files(&folder),
            project_files,
            "the project's folder changed"
        );
        assert_eq!(
            ktask_roots
                .iter()
                .flat_map(|root| files(root))
                .collect::<Vec<_>>(),
            kept_files
        );
        let state = &f.paths.state;
        for gone in [
            format!("evidence/attempt-{doomed_attempt}"),
            format!("runner-{doomed}.log"),
            format!("runner-{doomed}.log.1"),
            format!("runners/{doomed}.lock"),
            format!("provision/{doomed}.json"),
            format!("provision/{doomed}.lock"),
        ] {
            assert!(!state.join(&gone).exists(), "{gone} was kept");
        }
        for stays in [
            format!("evidence/attempt-{kept_attempt}/step-1/file"),
            format!("runner-{kept}.log"),
            format!("runner-{kept}.log.1"),
            format!("runners/{kept}.lock"),
            format!("provision/{kept}.json"),
        ] {
            assert!(state.join(&stays).exists(), "{stays} was removed");
        }
        let again = remove(&mut db, &f.paths, doomed, "doomed").unwrap_err();
        assert!(again.to_string().contains("not registered"), "{again}");
        Ok(())
    }

    #[test]
    fn a_project_with_a_live_runner_is_neither_deleted_nor_moved() -> Result<()> {
        let f = fixture()?;
        let (id, _) = populated(&f, "busy")?;
        let elsewhere = f.paths.state.parent().unwrap().join("elsewhere");
        fs::create_dir(&elsewhere)?;
        let mut db = Database::open_in(&f.paths.state)?;
        let before = row_counts(&db)?;
        let runner = fs::File::open(registration::lease_file(&f.paths, id))?;
        runner.lock()?;
        for refused in [
            remove(&mut db, &f.paths, id, "busy").map(|_| ()),
            relocate(&mut db, &f.paths, id, &elsewhere).map(|_| ()),
        ] {
            let error = refused.unwrap_err().to_string();
            assert!(error.contains("live runner"), "{error}");
        }
        assert_eq!(row_counts(&db)?, before);
        assert!(f.paths.state.join(format!("runner-{id}.log")).exists());
        Ok(())
    }

    #[test]
    fn moving_a_project_keeps_its_tasks_and_refuses_unusable_folders() -> Result<()> {
        let f = fixture()?;
        let (id, _) = populated(&f, "first")?;
        let (other, _) = populated(&f, "second")?;
        let base = f.paths.state.parent().unwrap().to_path_buf();
        let mut db = Database::open_in(&f.paths.state)?;
        for (folder, reason) in [
            (base.join("second"), "already project second"),
            (base.join("absent"), "Cannot use"),
            (base.join("first"), "already in that folder"),
            (base.join("first/README"), "must be a directory"),
            (PathBuf::from("relative"), "absolute"),
        ] {
            let error = relocate(&mut db, &f.paths, id, &folder)
                .unwrap_err()
                .to_string();
            assert!(error.contains(reason), "{error}");
        }
        fs::rename(base.join("first"), base.join("renamed"))?;
        let root = relocate(&mut db, &f.paths, id, &base.join("renamed"))?;
        assert_eq!(root, base.join("renamed").canonicalize()?);
        let moved = Service::find(Database::open_in(&f.paths.state)?, &root)?.unwrap();
        assert_eq!(
            (moved.project_id, moved.project_name.as_str()),
            (id, "first")
        );
        assert_eq!(moved.project_root()?, root);
        let listed = crate::application::overview::projects(&db)?;
        let first = listed.iter().find(|project| project.id == id).unwrap();
        assert_eq!((first.queued, first.root.clone()), (2, root));
        assert!(listed.iter().any(|project| project.id == other));
        Ok(())
    }
}

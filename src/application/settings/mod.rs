//! Moving a project to another computer: its settings go into one file, as its tasks do.
//!
//! Export reads everything configured for the project that is not a task, with paths inside
//! the project written relative to its root. Import applies a file to a folder in one
//! transaction, registering the folder when it is not a project; a preview is that same
//! import rolled back. History, attempts, evidence, usage, cost and what belongs to a
//! computer (installed tools, tool registrations, credentials) are not part of the file.
use super::{Actor, Service, configuration, preferences, register, require, retention};
use crate::{
    build_cache,
    instance::Paths,
    runner::{registration, setup, workspace},
    storage::{self, Database, Error, Result},
};
use file::Effect;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

mod apply;
mod collect;
mod file;
mod folder;

pub use file::{Entry, FORMAT, PreferenceValues, SettingsFile, VERSION};
pub use folder::PathNote;

/// The settings of a project as text, and what that text holds.
pub struct Export {
    pub text: String,
    pub written: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Added,
    Changed,
    Unchanged,
}

/// What import does with one setting of the file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Change {
    pub setting: String,
    pub status: Status,
    pub value: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Value>,
    #[serde(skip)]
    effect: Effect,
}

/// An import, done or previewed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub applied: bool,
    /// The folder the settings apply to.
    pub folder: PathBuf,
    pub project: String,
    /// The folder was not a project yet.
    pub registers_project: bool,
    pub changes: Vec<Change>,
    /// Paths of the file written for this folder.
    pub rewritten: Vec<PathNote>,
    /// Absolute paths outside the project, kept for the owner to confirm.
    pub outside: Vec<PathNote>,
    /// What the settings need that this computer lacks.
    pub missing: Vec<String>,
    /// Why applying is refused right now; only a preview carries it.
    pub blocked: Option<String>,
}

impl Report {
    /// Whether the import adds or changes any setting.
    pub fn changes_anything(&self) -> bool {
        self.changes
            .iter()
            .any(|change| change.status != Status::Unchanged)
    }
}

/// How each setting `file` names compares with what `current` has.
pub fn compare(file: &SettingsFile, current: &SettingsFile) -> Vec<Change> {
    let current = current.entries();
    file.entries()
        .into_iter()
        .map(|entry| {
            let previous = current.iter().find(|old| entry.same_setting(old));
            let status = match previous {
                None => Status::Added,
                Some(old) if same_value(&entry, old) => Status::Unchanged,
                Some(_) => Status::Changed,
            };
            Change {
                status,
                previous: previous
                    .filter(|_| status == Status::Changed)
                    .map(|old| old.value.clone()),
                setting: entry.setting,
                value: entry.value,
                effect: entry.effect,
            }
        })
        .collect()
}

/// Names that import matches without regard to case compare the same way.
fn same_value(new: &Entry, old: &Entry) -> bool {
    let folded = |value: &Value| {
        let mut value = value.clone();
        if let Some(name) = value.get_mut("name")
            && new.effect != Effect::Check
        {
            *name = name.as_str().unwrap_or_default().to_lowercase().into();
        }
        if matches!(
            new.setting.as_str(),
            "default lenses" | "selected profile" | "shared user default default_profile"
        ) {
            value = value.to_string().to_lowercase().into();
        }
        value
    };
    folded(&new.value) == folded(&old.value)
}

impl Service {
    /// This project's settings. Only reads, so it works while the runner is running.
    pub fn export_settings(&self, paths: &Paths) -> Result<Export> {
        let root = self.project_root()?;
        let preferences = self.preferences(&paths.config)?;
        let cache = build_cache::policy(paths)?;
        // One read transaction, so the file never mixes two states of the settings.
        let snapshot = self.db.connection().unchecked_transaction()?;
        let current = collect::collect(&snapshot, self.project_id, &preferences, cache)?;
        let mut settings = folder::to_placeholders(&current, &root)?;
        settings.source_folder = root.to_str().map(str::to_owned);
        Ok(Export {
            written: settings.entries(),
            text: settings.encode(),
        })
    }

    /// What importing `data` into this project would do; changes nothing.
    pub fn preview_settings(&mut self, paths: &Paths, data: &[u8]) -> Result<Report> {
        self.run_settings_import(paths, data, Mode::Preview)
    }

    /// Imports `data` into this project if it still does what `previewed` showed.
    pub fn import_settings(
        &mut self,
        paths: &Paths,
        data: &[u8],
        previewed: &Report,
    ) -> Result<Report> {
        self.run_settings_import(paths, data, Mode::Apply(Some(previewed)))
    }

    fn run_settings_import(
        &mut self,
        paths: &Paths,
        data: &[u8],
        mode: Mode<'_>,
    ) -> Result<Report> {
        let root = self.project_root()?;
        let request = Request {
            paths,
            folder: &root,
            name: None,
            actor: &self.actor,
            data,
        };
        request.run(self.db.connection_mut(), mode)
    }
}

/// A settings file to import into the project of `folder`.
pub struct Request<'a> {
    pub paths: &'a Paths,
    pub folder: &'a Path,
    /// The name for a folder that is not a project yet.
    pub name: Option<&'a str>,
    pub actor: &'a Actor,
    pub data: &'a [u8],
}

#[derive(Clone, Copy)]
enum Mode<'a> {
    Preview,
    Apply(Option<&'a Report>),
}

impl Request<'_> {
    /// What the import would do. An instance without storage is not given any.
    pub fn preview(&self) -> Result<Report> {
        match Database::open_existing(self.paths)? {
            Some(mut db) => self.run(db.connection_mut(), Mode::Preview),
            None => self.run(&mut storage::empty_instance()?, Mode::Preview),
        }
    }

    /// Applies the file: everything or nothing.
    pub fn import(&self) -> Result<Report> {
        // A file that cannot be read must not create storage.
        SettingsFile::decode(self.data)?;
        let root = repository(self.folder)?;
        let mut db = Database::open(self.paths)?;
        retention::initialize_capacity_admission(&db)?;
        Request {
            folder: &root,
            ..*self
        }
        .run(db.connection_mut(), Mode::Apply(None))
    }

    /// The import under one transaction, which only [`Mode::Apply`] commits.
    fn run(&self, db: &mut Connection, mode: Mode<'_>) -> Result<Report> {
        let named = SettingsFile::decode(self.data)?;
        let root = repository(self.folder)?;
        let (rewritten, outside) = folder::paths(&named, &root)?;
        let settings = folder::to_folder(&named, &root)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let target = Target::of(&tx, &root, self.name)?;
        let files = FileSettings::read(&tx, self.paths, target.id)?;
        let next_files = files.with(&settings)?;
        let current = collect::collect(&tx, target.id, &files.preferences, files.build_cache)?;
        let changes = compare(&settings, &current);
        // Held until the import ends, so no runner starts in between.
        let (_runner, blocked) = admit(&tx, self.paths, &target, &changes, mode)?;
        let project = apply::Project {
            id: target.id,
            root: &root,
            actor: self.actor,
        };
        apply::apply(&tx, &project, &settings)?;
        let needed_models = apply::needed_models(&tx, target.id, &settings)?;
        let needed_checks = configuration::checks(&tx, target.id)?;
        let mut report = Report {
            applied: false,
            folder: root.clone(),
            project: target.name,
            registers_project: target.registers,
            changes,
            rewritten,
            outside,
            missing: Vec::new(),
            blocked,
        };
        if let Mode::Apply(previewed) = mode {
            if let Some(previewed) = previewed {
                require(
                    previewed.changes == report.changes && previewed.rewritten == report.rewritten,
                    "Settings or the file changed since the preview; preview it again",
                )?;
            }
            let restore = files.write(&tx, self.paths, target.id, &next_files)?;
            tx.commit()?;
            restore.keep();
            report.applied = true;
        } else {
            // Dropping the transaction rolls a preview's whole import back.
            drop(tx);
        }
        // Probing the host can take a while, so it runs after the database is released.
        report.missing = setup::missing_on_host(&root, &needed_models, &needed_checks);
        report.missing.extend(unbound_tools(&settings, &current));
        Ok(report)
    }
}

/// The canonical `folder`, which must be the root of a Git repository.
fn repository(folder: &Path) -> Result<PathBuf> {
    let refused = || {
        Error::Conflict(format!(
            "{} is not the root of a Git repository; clone the project first, or run git init there",
            folder.display()
        ))
    };
    let root = folder.canonicalize().map_err(|_| refused())?;
    let top = workspace::git(&root, &["rev-parse", "--show-toplevel"]).map_err(|_| refused())?;
    if Path::new(top.trim()).canonicalize().ok().as_ref() == Some(&root) {
        Ok(root)
    } else {
        Err(refused())
    }
}

struct Target {
    id: i64,
    name: String,
    registers: bool,
}

impl Target {
    /// The project of `root`, registered under the caller's transaction when it is none yet.
    fn of(tx: &Transaction<'_>, root: &Path, name: Option<&str>) -> Result<Self> {
        let found: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, name FROM projects WHERE root_path = ?1",
                [root.as_os_str().as_encoded_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let registers = found.is_none();
        let (id, stored) = match found {
            Some(project) => {
                require(
                    name.is_none_or(|name| name == project.1),
                    "Project already has a different name",
                )?;
                project
            }
            None => register(tx, root, name)?,
        };
        Ok(Self {
            id,
            name: stored,
            registers,
        })
    }
}

/// Applying is refused while execution uses what would change; a preview says why instead.
fn admit(
    db: &Connection,
    paths: &Paths,
    target: &Target,
    changes: &[Change],
    mode: Mode<'_>,
) -> Result<(Option<registration::Claim>, Option<String>)> {
    match (idle(db, paths, target, changes, mode), mode) {
        (Err(Error::Conflict(why)), Mode::Preview) => Ok((None, Some(why))),
        (claim, _) => Ok((claim?, None)),
    }
}

/// When the import changes what a live attempt uses, the runner must be stopped; checks
/// and delivery also need the attempt to be over, as their own commands do.
fn idle(
    db: &Connection,
    paths: &Paths,
    target: &Target,
    changes: &[Change],
    mode: Mode<'_>,
) -> Result<Option<registration::Claim>> {
    let changed = |matches: fn(Effect) -> bool| {
        changes
            .iter()
            .any(|change| change.status != Status::Unchanged && matches(change.effect))
    };
    if target.registers || !changed(|effect| effect != Effect::Later) {
        return Ok(None);
    }
    let alive = || {
        Error::Conflict(format!(
            "{} has a live runner and the file changes settings a running attempt uses · pause it first (ktask pause)",
            target.name
        ))
    };
    let claim = match mode {
        Mode::Preview => {
            if registration::active(paths, target.id)?.is_some() {
                return Err(alive());
            }
            None
        }
        Mode::Apply(_) => Some(registration::claim(paths, target.id)?.ok_or_else(alive)?),
    };
    if changed(|effect| effect == Effect::Check) {
        configuration::require_checks_idle(db, target.id)?;
    }
    if changed(|effect| effect == Effect::Delivery) {
        configuration::require_delivery_idle(db, target.id)?;
    }
    Ok(claim)
}

fn unbound_tools(settings: &SettingsFile, current: &SettingsFile) -> Vec<String> {
    settings
        .tools
        .iter()
        .filter(|needed| !current.tools.contains(needed))
        .map(|needed| {
            format!(
                "tool {} {} is not bound to this project; register and bind it (ktask tool)",
                needed.name, needed.version
            )
        })
        .collect()
}

/// The settings kept in files beside the database.
struct FileSettings {
    preferences: preferences::Preferences,
    build_cache: build_cache::Policy,
}

impl FileSettings {
    fn read(db: &Connection, paths: &Paths, project: i64) -> Result<Self> {
        Ok(Self {
            preferences: preferences::read(db, project, &paths.config)?,
            build_cache: build_cache::policy(paths)?,
        })
    }

    /// These with what `settings` names in their place, checked like any change.
    fn with(&self, settings: &SettingsFile) -> Result<Self> {
        let values = &settings.project.preferences;
        let mut preferences = self.preferences.clone();
        preferences.mouse = values.mouse.unwrap_or(preferences.mouse);
        preferences.continuation = values.run_mode.unwrap_or(preferences.continuation);
        preferences.planner_approval = values
            .planner_approval
            .unwrap_or(preferences.planner_approval);
        let values = &settings.shared.build_cache;
        let mut build_cache = self.build_cache;
        build_cache.max_bytes = values.max_bytes.unwrap_or(build_cache.max_bytes);
        build_cache.max_files = values.max_files.unwrap_or(build_cache.max_files);
        build_cache.keep_generations = values
            .keep_generations
            .unwrap_or(build_cache.keep_generations);
        build_cache.grace_secs = values.grace_secs.unwrap_or(build_cache.grace_secs);
        build_cache.min_free_bytes = values.min_free_bytes.unwrap_or(build_cache.min_free_bytes);
        build_cache.validate()?;
        Ok(Self {
            preferences,
            build_cache,
        })
    }

    /// Saves what differs in `next`; the returned guard undoes it unless kept.
    fn write(&self, db: &Connection, paths: &Paths, project: i64, next: &Self) -> Result<Restore> {
        let mut restore = Restore::default();
        if next.preferences != self.preferences {
            restore.remember(preferences::path(&paths.config, project))?;
            preferences::save(
                db,
                project,
                &paths.config,
                &self.preferences,
                &next.preferences,
            )?;
        }
        if next.build_cache != self.build_cache {
            restore.remember(paths.config.join(build_cache::POLICY_FILE))?;
            build_cache::set_policy(paths, self.build_cache, next.build_cache)?;
        }
        Ok(restore)
    }
}

/// Files as they were before an import wrote them, put back unless it commits.
#[derive(Default)]
struct Restore(Vec<(PathBuf, Option<Vec<u8>>)>);

impl Restore {
    fn remember(&mut self, path: PathBuf) -> Result<()> {
        let before = match fs::read(&path) {
            Ok(data) => Some(data),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        self.0.push((path, before));
        Ok(())
    }

    fn keep(mut self) {
        self.0.clear();
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, before) in self.0.drain(..) {
            let restored = match before {
                Some(data) => fs::write(&path, data),
                None => fs::remove_file(&path),
            };
            if let Err(error) = restored
                && error.kind() != io::ErrorKind::NotFound
            {
                note!("Could not restore {}: {error}", path.display());
            }
        }
    }
}

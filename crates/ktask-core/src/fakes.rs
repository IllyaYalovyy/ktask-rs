//! In-memory implementations of the ports, for the tests of this crate.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{
    AppendConflict, Clock, CommandSpec, Commands, CommandsError, Event, Git, GitError, Journal,
    JournalError, Output, Project, ProjectRegistry, RegistryError, RunLock, RunLockError, Settings,
    SettingsError, SettingsStore, TaskDraft, TaskKind,
};

/// An in-memory registry that can be told to fail.
#[derive(Debug, Default)]
pub(crate) struct FakeRegistry {
    pub(crate) projects: RefCell<Vec<Project>>,
    pub(crate) failure: Option<RegistryError>,
}

impl FakeRegistry {
    pub(crate) fn with(projects: Vec<Project>) -> Self {
        Self {
            projects: RefCell::new(projects),
            failure: None,
        }
    }

    pub(crate) fn failing(failure: RegistryError) -> Self {
        Self {
            projects: RefCell::default(),
            failure: Some(failure),
        }
    }
}

impl ProjectRegistry for FakeRegistry {
    fn list(&self) -> Result<Vec<Project>, RegistryError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.projects.borrow().clone()),
        }
    }

    fn add(&self, project: &Project) -> Result<(), RegistryError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        self.projects.borrow_mut().push(project.clone());
        Ok(())
    }
}

/// A git that knows the work trees it was given.
#[derive(Debug, Default)]
pub(crate) struct FakeGit {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) failure: Option<GitError>,
}

impl Git for FakeGit {
    fn work_tree_root(&self, dir: &Path) -> Result<Option<PathBuf>, GitError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        Ok(self
            .roots
            .iter()
            .filter(|root| dir.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned())
    }
}

/// A clock stopped at a moment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FakeClock(pub(crate) SystemTime);

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// `seconds` after the epoch.
pub(crate) fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A project named `name`, in `/work/<name>`, registered `seconds` after the epoch.
pub(crate) fn project(name: &str, seconds: u64) -> Project {
    Project {
        name: name.to_owned(),
        path: PathBuf::from(format!("/work/{name}")),
        registered_at: at(seconds),
    }
}

/// An in-memory journal that can be told to fail: [`Journal::events`] and
/// [`Journal::append_events`] are all there is to it — the one source of truth for the
/// queue's state, tasks and attempts alike, is `events`, exactly as
/// [`crate::queue_state::QueueState`] folds it.
#[derive(Debug, Default)]
pub(crate) struct FakeJournal {
    pub(crate) events: RefCell<Vec<Event>>,
    pub(crate) failure: Option<JournalError>,
}

impl FakeJournal {
    pub(crate) fn failing(failure: JournalError) -> Self {
        Self {
            events: RefCell::default(),
            failure: Some(failure),
        }
    }
}

impl Journal for FakeJournal {
    fn events(&self) -> Result<Vec<Event>, JournalError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.events.borrow().clone()),
        }
    }

    fn append_events(&self, events: &[Event], read: usize) -> Result<(), AppendConflict> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        if self.events.borrow().len() != read {
            return Err(AppendConflict::Conflict);
        }
        self.events.borrow_mut().extend_from_slice(events);
        Ok(())
    }
}

/// A commands port that records the last spec it was given and always returns the same
/// canned result.
#[derive(Debug)]
pub(crate) struct FakeCommands {
    pub(crate) last: RefCell<Option<CommandSpec>>,
    result: Result<Output, CommandsError>,
}

impl FakeCommands {
    pub(crate) fn returning(result: Result<Output, CommandsError>) -> Self {
        Self {
            last: RefCell::default(),
            result,
        }
    }
}

impl Commands for FakeCommands {
    fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
        *self.last.borrow_mut() = Some(spec.clone());
        self.result.clone()
    }
}

/// A run lock that either always succeeds or always fails the same way.
#[derive(Debug)]
pub(crate) struct FakeRunLock {
    failure: Option<RunLockError>,
}

impl FakeRunLock {
    pub(crate) fn free() -> Self {
        Self { failure: None }
    }

    pub(crate) fn held_by(pid: Option<u32>) -> Self {
        Self {
            failure: Some(RunLockError::InProgress(pid)),
        }
    }
}

impl RunLock for FakeRunLock {
    fn acquire(&self) -> Result<(), RunLockError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }

    fn in_progress(&self) -> Result<bool, RunLockError> {
        Ok(self.failure.is_some())
    }
}

/// An in-memory settings store that can be told to fail.
#[derive(Debug, Default)]
pub(crate) struct FakeSettingsStore {
    pub(crate) settings: RefCell<Settings>,
    pub(crate) failure: Option<SettingsError>,
}

impl FakeSettingsStore {
    pub(crate) fn with(settings: Settings) -> Self {
        Self {
            settings: RefCell::new(settings),
            failure: None,
        }
    }

    pub(crate) fn failing(failure: SettingsError) -> Self {
        Self {
            settings: RefCell::default(),
            failure: Some(failure),
        }
    }
}

impl SettingsStore for FakeSettingsStore {
    fn load(&self) -> Result<Settings, SettingsError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.settings.borrow().clone()),
        }
    }

    fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        *self.settings.borrow_mut() = settings.clone();
        Ok(())
    }
}

/// A valid draft titled `title`, with one criterion and nothing else.
pub(crate) fn draft(title: &str) -> TaskDraft {
    TaskDraft {
        title: title.to_owned(),
        body: String::new(),
        criteria: vec!["it works".to_owned()],
        kind: TaskKind::Agent,
        links: vec![],
    }
}

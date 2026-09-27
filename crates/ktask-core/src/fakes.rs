//! In-memory implementations of the ports, for the tests of this crate.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{
    Clock, Git, GitError, Journal, JournalError, Project, ProjectRegistry, RegistryError, Task,
    TaskDraft, TaskId, TaskKind, TaskStatus,
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

/// An in-memory journal that can be told to fail.
#[derive(Debug, Default)]
pub(crate) struct FakeJournal {
    pub(crate) tasks: RefCell<Vec<Task>>,
    pub(crate) failure: Option<JournalError>,
}

impl FakeJournal {
    pub(crate) fn failing(failure: JournalError) -> Self {
        Self {
            tasks: RefCell::default(),
            failure: Some(failure),
        }
    }
}

impl Journal for FakeJournal {
    fn append_task(&self, draft: &TaskDraft, at: SystemTime) -> Result<Task, JournalError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let mut tasks = self.tasks.borrow_mut();
        let task = Task {
            id: TaskId(tasks.len() as u64 + 1),
            position: tasks.len() + 1,
            title: draft.title.clone(),
            body: draft.body.clone(),
            criteria: draft.criteria.clone(),
            kind: draft.kind,
            links: draft.links.clone(),
            status: TaskStatus::Pending,
            created_at: at,
        };
        tasks.push(task.clone());
        Ok(task)
    }

    fn tasks(&self) -> Result<Vec<Task>, JournalError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.tasks.borrow().clone()),
        }
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

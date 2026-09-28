//! In-memory implementations of the ports, for the tests of this crate.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{
    AppendError, Attempt, AttemptEnd, AttemptRun, BeginAttemptError, CancelError, Clock,
    CommandSpec, Commands, CommandsError, Git, GitError, Journal, JournalError, Outcome, Output,
    Placement, Project, ProjectRegistry, RecordReportError, RegistryError, RunLock, RunLockError,
    Task, TaskDraft, TaskId, TaskKind, TaskStatus,
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
    pub(crate) attempts: RefCell<HashMap<TaskId, u32>>,
    pub(crate) reports: RefCell<HashMap<(TaskId, u32), Report>>,
    pub(crate) started: RefCell<HashMap<(TaskId, u32), SystemTime>>,
    pub(crate) providers: RefCell<HashMap<(TaskId, u32), String>>,
    pub(crate) ended: RefCell<HashMap<(TaskId, u32), AttemptEnd>>,
    pub(crate) failure: Option<JournalError>,
}

/// What the agent reported for one attempt, as [`FakeJournal`] keeps it.
type Report = (Outcome, Option<String>);

impl FakeJournal {
    pub(crate) fn failing(failure: JournalError) -> Self {
        Self {
            tasks: RefCell::default(),
            attempts: RefCell::default(),
            reports: RefCell::default(),
            started: RefCell::default(),
            providers: RefCell::default(),
            ended: RefCell::default(),
            failure: Some(failure),
        }
    }
}

impl Journal for FakeJournal {
    fn append_tasks(
        &self,
        drafts: &[TaskDraft],
        placement: Placement,
        at: SystemTime,
    ) -> Result<Vec<Task>, AppendError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        // Worked out on a copy, so that a failure part-way leaves the queue as it was.
        let mut tasks = self.tasks.borrow().clone();
        let mut added = Vec::new();
        let mut placement = placement;
        for draft in drafts {
            let next = |anchor: TaskId| {
                let index = tasks
                    .iter()
                    .position(|task| task.id == anchor)
                    .ok_or(AppendError::UnknownTask(anchor))?;
                if tasks[index].status == TaskStatus::Cancelled {
                    return Err(AppendError::CancelledTask(anchor));
                }
                Ok(index)
            };
            let index = match placement {
                Placement::End => tasks.len(),
                Placement::Before(anchor) => next(anchor)?,
                Placement::After(anchor) => next(anchor)? + 1,
            };
            let task = Task {
                id: TaskId(tasks.len() as u64 + 1),
                position: index + 1,
                title: draft.title.clone(),
                body: draft.body.clone(),
                criteria: draft.criteria.clone(),
                kind: draft.kind,
                links: draft.links.clone(),
                status: TaskStatus::Pending,
                created_at: at,
            };
            tasks.insert(index, task.clone());
            for (index, task) in tasks.iter_mut().enumerate() {
                task.position = index + 1;
            }
            placement = placement.then_after(task.id);
            added.push(task);
        }
        *self.tasks.borrow_mut() = tasks;
        Ok(added)
    }

    fn cancel_task(&self, id: TaskId, _at: SystemTime) -> Result<(), CancelError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        let mut tasks = self.tasks.borrow_mut();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or(CancelError::UnknownTask(id))?;
        if task.status == TaskStatus::Cancelled {
            return Err(CancelError::AlreadyCancelled(id));
        }
        task.status = TaskStatus::Cancelled;
        Ok(())
    }

    fn tasks(&self) -> Result<Vec<Task>, JournalError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.tasks.borrow().clone()),
        }
    }

    fn begin_attempt(&self, id: TaskId, at: SystemTime) -> Result<u32, BeginAttemptError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        let mut tasks = self.tasks.borrow_mut();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or(BeginAttemptError::UnknownTask(id))?;
        if task.status != TaskStatus::Pending {
            return Err(BeginAttemptError::NotPending(id));
        }
        task.status = TaskStatus::Running;
        let mut attempts = self.attempts.borrow_mut();
        let number = attempts.entry(id).or_insert(0);
        *number += 1;
        self.started.borrow_mut().insert((id, *number), at);
        Ok(*number)
    }

    fn record_report(
        &self,
        id: TaskId,
        number: u32,
        outcome: Outcome,
        reason: Option<&str>,
        _at: SystemTime,
    ) -> Result<(), RecordReportError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        let tasks = self.tasks.borrow();
        let Some(task) = tasks.iter().find(|task| task.id == id) else {
            return Err(RecordReportError::UnknownAttempt { task: id, number });
        };
        let current = self.attempts.borrow().get(&id).copied().unwrap_or(0);
        if current != number {
            return Err(RecordReportError::UnknownAttempt { task: id, number });
        }
        if task.status != TaskStatus::Running {
            return Err(RecordReportError::AttemptEnded { task: id, number });
        }
        self.reports
            .borrow_mut()
            .insert((id, number), (outcome, reason.map(str::to_owned)));
        Ok(())
    }

    fn attempt_running(
        &self,
        id: TaskId,
        number: u32,
        provider: &str,
        _at: SystemTime,
    ) -> Result<(), JournalError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        self.providers
            .borrow_mut()
            .insert((id, number), provider.to_owned());
        Ok(())
    }

    fn last_report(
        &self,
        id: TaskId,
        number: u32,
    ) -> Result<Option<(Outcome, Option<String>)>, JournalError> {
        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.reports.borrow().get(&(id, number)).cloned()),
        }
    }

    fn end_attempt(
        &self,
        id: TaskId,
        number: u32,
        run: AttemptRun<'_>,
        _at: SystemTime,
    ) -> Result<(), RecordReportError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        let mut tasks = self.tasks.borrow_mut();
        let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
            return Err(RecordReportError::UnknownAttempt { task: id, number });
        };
        let current = self.attempts.borrow().get(&id).copied().unwrap_or(0);
        if current != number {
            return Err(RecordReportError::UnknownAttempt { task: id, number });
        }
        if task.status != TaskStatus::Running {
            return Err(RecordReportError::AttemptEnded { task: id, number });
        }
        task.status = run.status;
        self.ended.borrow_mut().insert(
            (id, number),
            AttemptEnd {
                duration: run.duration,
                status: run.status,
                reason: run.reason.map(str::to_owned),
            },
        );
        Ok(())
    }

    fn running(&self) -> Result<Option<(TaskId, u32)>, JournalError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let tasks = self.tasks.borrow();
        let Some(task) = tasks.iter().find(|task| task.status == TaskStatus::Running) else {
            return Ok(None);
        };
        let number = self.attempts.borrow().get(&task.id).copied().unwrap_or(0);
        Ok(Some((task.id, number)))
    }

    fn last_attempt(&self, id: TaskId) -> Result<Option<Attempt>, JournalError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let Some(&number) = self.attempts.borrow().get(&id) else {
            return Ok(None);
        };
        let Some(&started_at) = self.started.borrow().get(&(id, number)) else {
            return Ok(None);
        };
        let provider = self.providers.borrow().get(&(id, number)).cloned();
        let ended = self.ended.borrow().get(&(id, number)).cloned();
        Ok(Some(Attempt {
            number,
            started_at,
            provider,
            ended,
        }))
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

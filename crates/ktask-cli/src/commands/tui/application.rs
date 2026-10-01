//! [`CliApplication`]: the one value every use case the terminal interface reaches is called
//! through, wired to whichever project is active — and [`drive`], which builds it and runs
//! the terminal interface with it until the operator quits.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use ktask_adapters::{
    FileJournalWatch, FileRunLock, GitCli, SqliteJournal, SqliteRegistry, SystemClock,
    TomlSettingsStore,
};
use ktask_core::{
    AddError, AnswerError, CancelError, DoneError, ForgetError, Import, JournalError, Placement,
    Project, QueueView, RegisterError, RegistryError, RetryError, RunReport, SetSettingError,
    SettingView, SettingsError, TaskDraft, TaskId,
};
use ktask_tui::Application;

use crate::context::{journal_file, open_settings_store, run_lock_file, state_root};

use super::process::{RunRefusal as ProcessRefusal, start_run};
use super::{ImportProblem, Start, import_into};

/// Why opening a project's state failed: an environment problem — the same kind every other
/// command already reports as text, since there is no further structure to it — or the journal
/// could not be used, keeping that error's own meaning until it is shown.
#[derive(Debug)]
pub(super) enum OpenProjectError {
    Environment(String),
    Journal(JournalError),
}

impl fmt::Display for OpenProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(message) => f.write_str(message),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

/// Everything wired to whichever project is active: its journal, run lock and settings —
/// reopened fresh each time the operator switches to another one, or once the current directory
/// is registered under a name it was asked for, so every action from then on applies to it.
pub(super) struct ProjectContext {
    project: Project,
    journal: SqliteJournal,
    lock: FileRunLock,
    settings_store: TomlSettingsStore,
}

impl ProjectContext {
    pub(super) fn open(project: Project) -> Result<Self, OpenProjectError> {
        let journal =
            SqliteJournal::open(&journal_file(&project).map_err(OpenProjectError::Environment)?)
                .map_err(OpenProjectError::Journal)?;
        let lock =
            FileRunLock::new(run_lock_file(&project).map_err(OpenProjectError::Environment)?);
        let settings_store =
            open_settings_store(&project).map_err(OpenProjectError::Environment)?;
        Ok(Self {
            project,
            journal,
            lock,
            settings_store,
        })
    }
}

/// `project`'s context, opened fresh, with its queue — the two steps that always go together
/// when the active project changes, whether by switching to another registered one or by
/// registering the current directory under a name it was asked for.
fn opened(project: Project) -> Result<(ProjectContext, QueueView), OpenProjectError> {
    let context = ProjectContext::open(project.clone())?;
    let view = ktask_core::queue_view(
        project,
        &context.journal,
        &SystemClock,
        &context.lock,
        false,
    )
    .map_err(OpenProjectError::Journal)?;
    Ok((context, view))
}

/// Why an action that needs a project open failed: none is open yet — only possible before the
/// registration screen's name is submitted — or its own use case refused, keeping that error's
/// own meaning until it is shown.
#[derive(Debug)]
pub(super) enum NeedsProject<E> {
    NoProject,
    Failed(E),
}

impl<E: fmt::Display> fmt::Display for NeedsProject<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProject => f.write_str("no project is open yet"),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

/// Why [`CliApplication::switch_project`] failed.
#[derive(Debug)]
pub(super) enum SwitchProjectError {
    List(RegistryError),
    Unknown(String),
    Open(OpenProjectError),
}

impl fmt::Display for SwitchProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::List(error) => error.fmt(f),
            Self::Unknown(name) => write!(f, "unknown project {name:?}"),
            Self::Open(error) => error.fmt(f),
        }
    }
}

/// Why [`CliApplication::forget_project`] failed.
#[derive(Debug)]
pub(super) enum ForgetProjectError {
    Forget(ForgetError),
    List(RegistryError),
}

impl fmt::Display for ForgetProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forget(error) => error.fmt(f),
            Self::List(error) => error.fmt(f),
        }
    }
}

/// Why [`CliApplication::register`] failed.
#[derive(Debug)]
pub(super) enum RegisterProjectError {
    Register(RegisterError),
    Open(OpenProjectError),
}

impl fmt::Display for RegisterProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(error) => error.fmt(f),
            Self::Open(error) => error.fmt(f),
        }
    }
}

/// The one value every use case the terminal interface reaches is called through, wired to
/// whichever project is active. Held behind `Mutex`es, not `RefCell`s, because `start_run` runs
/// on a thread of its own so the screen stays responsive for however long a run takes, while
/// every other action runs on the loop's own thread — both reach the same state.
struct CliApplication {
    context: Mutex<Option<ProjectContext>>,
    active_project: Mutex<Option<Project>>,
    registry: Mutex<SqliteRegistry>,
    cwd: PathBuf,
    binary_path: PathBuf,
}

impl CliApplication {
    /// Runs `with` against the active project's context, refusing with [`NeedsProject::NoProject`]
    /// when none is open yet.
    fn with_context<T, E>(
        &self,
        with: impl FnOnce(&ProjectContext) -> Result<T, E>,
    ) -> Result<T, NeedsProject<E>> {
        let guard = self.context.lock().unwrap_or_else(PoisonError::into_inner);
        let context = guard.as_ref().ok_or(NeedsProject::NoProject)?;
        with(context).map_err(NeedsProject::Failed)
    }
}

impl Application for CliApplication {
    type LoadError = NeedsProject<JournalError>;
    type RemoveError = NeedsProject<CancelError>;
    type RetryError = NeedsProject<RetryError>;
    type AnswerError = NeedsProject<AnswerError>;
    type DoneError = NeedsProject<DoneError>;
    type AddProblem = NeedsProject<AddError>;
    type SettingsError = NeedsProject<SettingsError>;
    type SaveSettingError = NeedsProject<SetSettingError>;
    type ProjectsError = RegistryError;
    type SwitchError = SwitchProjectError;
    type ForgetError = ForgetProjectError;
    type RegisterError = RegisterProjectError;
    type ImportError = NeedsProject<ImportProblem>;
    type RunRefusal = NeedsProject<ProcessRefusal>;

    fn load_queue(&self, show_cancelled: bool) -> Result<QueueView, Self::LoadError> {
        self.with_context(|context| {
            ktask_core::queue_view(
                context.project.clone(),
                &context.journal,
                &SystemClock,
                &context.lock,
                show_cancelled,
            )
        })
    }

    fn remove_task(&self, id: TaskId) -> Result<(), Self::RemoveError> {
        self.with_context(|context| ktask_core::remove_task(&context.journal, &SystemClock, id))
    }

    fn retry_task(&self, id: TaskId) -> Result<(), Self::RetryError> {
        self.with_context(|context| ktask_core::retry_task(&context.journal, &SystemClock, id))
    }

    fn answer_task(&self, id: TaskId, text: &str) -> Result<(), Self::AnswerError> {
        self.with_context(|context| {
            ktask_core::answer_task(&context.journal, &SystemClock, id, text)
        })
    }

    fn done_task(&self, id: TaskId, reason: &str) -> Result<(), Self::DoneError> {
        self.with_context(|context| {
            ktask_core::done_task(&context.journal, &SystemClock, id, reason)
        })
    }

    fn add_task(
        &self,
        draft: &TaskDraft,
        placement: Placement,
    ) -> Result<TaskId, Vec<Self::AddProblem>> {
        let guard = self.context.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(context) = guard.as_ref() else {
            return Err(vec![NeedsProject::NoProject]);
        };
        ktask_core::add_task(&context.journal, &SystemClock, draft, placement)
            .map(|task| task.id)
            .map_err(|errors| errors.into_iter().map(NeedsProject::Failed).collect())
    }

    fn load_settings(&self) -> Result<Vec<SettingView>, Self::SettingsError> {
        self.with_context(|context| ktask_core::show_settings(&context.settings_store))
    }

    fn save_setting(&self, name: &str, value: &str) -> Result<SettingView, Self::SaveSettingError> {
        self.with_context(|context| {
            ktask_core::set_setting(
                &context.settings_store,
                &GitCli,
                &context.project.path,
                name,
                value,
            )
        })
    }

    fn import(&self, path: &str) -> Result<Import, Self::ImportError> {
        self.with_context(|context| import_into(&context.journal, path))
    }

    fn load_projects(&self) -> Result<Vec<Project>, Self::ProjectsError> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        ktask_core::list_projects(&*registry)
    }

    fn switch_project(&self, name: &str) -> Result<QueueView, Self::SwitchError> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let projects = ktask_core::list_projects(&*registry).map_err(SwitchProjectError::List)?;
        let project = projects
            .into_iter()
            .find(|project| project.name == name)
            .ok_or_else(|| SwitchProjectError::Unknown(name.to_owned()))?;
        let (fresh, view) = opened(project.clone()).map_err(SwitchProjectError::Open)?;
        *self.context.lock().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
        *self
            .active_project
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(project);
        Ok(view)
    }

    fn forget_project(&self, name: &str) -> Result<Vec<Project>, Self::ForgetError> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        ktask_core::forget_project(&*registry, name).map_err(ForgetProjectError::Forget)?;
        ktask_core::list_projects(&*registry).map_err(ForgetProjectError::List)
    }

    fn register(&self, name: &str) -> Result<QueueView, Self::RegisterError> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let project =
            ktask_core::register_project(&*registry, &GitCli, &SystemClock, &self.cwd, name)
                .map_err(RegisterProjectError::Register)?;
        let (fresh, view) = opened(project.clone()).map_err(RegisterProjectError::Open)?;
        *self.context.lock().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
        *self
            .active_project
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(project);
        Ok(view)
    }

    fn start_run(&self) -> Result<RunReport, Self::RunRefusal> {
        let project = self
            .active_project
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match project {
            Some(project) => start_run(&self.binary_path, &project).map_err(NeedsProject::Failed),
            None => Err(NeedsProject::NoProject),
        }
    }
}

/// Wires every action the screen can take to the active project's journal, run lock and
/// settings, starting on `start`, and runs the terminal interface with it until the operator
/// quits. The journal watch covers every registered project's state, not just the active
/// project's, so a project switched to, or a directory registered, after the screen opened is
/// covered too.
pub(super) fn drive(
    registry: SqliteRegistry,
    cwd: PathBuf,
    start: Start,
    binary_path: PathBuf,
) -> Result<(), String> {
    let (initial_context, tui_start, initial_project) = match start {
        Start::Ready(project) => (
            Some(ProjectContext::open(project.clone()).map_err(|e| e.to_string())?),
            ktask_tui::Start::Ready,
            Some(project),
        ),
        Start::NameTaken(message) => (None, ktask_tui::Start::NameTaken { message }, None),
    };
    let watch = FileJournalWatch::open(&state_root()?, true).map_err(|e| e.to_string())?;
    let application = CliApplication {
        context: Mutex::new(initial_context),
        active_project: Mutex::new(initial_project),
        registry: Mutex::new(registry),
        cwd,
        binary_path,
    };
    ktask_tui::run(tui_start, application, watch)
}

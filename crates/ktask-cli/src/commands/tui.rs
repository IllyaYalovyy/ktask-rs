//! `ktask-rs tui`: open the terminal interface on the project's queue.

use std::fmt;
use std::io::{self, IsTerminal, Read};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;

use ktask_adapters::{
    FileJournalWatch, FileRunLock, GitCli, SqliteJournal, SqliteRegistry, SystemClock,
    TomlSettingsStore,
};
use ktask_core::{
    AddError, CancelError, ForgetError, JournalError, Placement, Project, QueueView, RegisterError,
    RegistryError, ResolveError, SetSettingError, SettingView, SettingsError, TaskDraft, TaskId,
};
use ktask_tui::Application;

use crate::context::{
    current_dir, current_exe, journal_file, merge_project, open_registry, open_settings_store,
    resolved, run_lock_file, state_root,
};
use crate::error::Failure;
use crate::render;

/// `ktask-rs tui`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// What the terminal interface opens on: the project's queue, already resolved, or a name still
/// needed to register the current directory under, because its own folder name is already
/// registered for another path.
enum Start {
    Ready(Project),
    NameTaken(String),
}

/// Opens the terminal interface on the queue of the project selected, or the current one — or,
/// when the current directory's own folder name is already taken by another registered path,
/// asks for a name to register it under instead of refusing outright, since there is a terminal
/// right here to ask on; the outcome either way is the one `ktask-rs project register --name`
/// gives for the same name.
pub(crate) fn run(args: &Args, project: Option<&str>) -> Result<(), Failure> {
    ensure_terminal()?;
    let registry = open_registry()?;
    let selected = merge_project(project, args.project.as_deref())?;
    let cwd = current_dir()?;
    let start = resolve_or_ask_to_register(&registry, &cwd, selected.as_deref())?;
    let binary_path = current_exe()?;
    Ok(drive(registry, cwd, start, binary_path)?)
}

/// Resolves the project the terminal interface opens on, exactly as every other command does,
/// except that a folder name already taken by another path is not refused outright: it is
/// handed to the screen instead, to ask for a name interactively.
fn resolve_or_ask_to_register(
    registry: &SqliteRegistry,
    cwd: &Path,
    selected: Option<&str>,
) -> Result<Start, Failure> {
    match ktask_core::resolve_project(registry, &GitCli, &SystemClock, cwd, selected) {
        Ok(resolution) => {
            let (project, _settings) = resolved(resolution)?;
            Ok(Start::Ready(project))
        }
        Err(error @ ResolveError::NameTaken { .. }) => Ok(Start::NameTaken(error.to_string())),
        Err(other) => Err(other.into()),
    }
}

/// Why opening a project's state failed: an environment problem — the same kind every other
/// command already reports as text, since there is no further structure to it — or the journal
/// could not be used, keeping that error's own meaning until it is shown.
#[derive(Debug)]
enum OpenProjectError {
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
struct ProjectContext {
    project: Project,
    journal: SqliteJournal,
    lock: FileRunLock,
    settings_store: TomlSettingsStore,
}

impl ProjectContext {
    fn open(project: Project) -> Result<Self, OpenProjectError> {
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
enum NeedsProject<E> {
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
enum SwitchProjectError {
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
enum ForgetProjectError {
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
enum RegisterProjectError {
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
    type AddProblem = NeedsProject<AddError>;
    type SettingsError = NeedsProject<SettingsError>;
    type SaveSettingError = NeedsProject<SetSettingError>;
    type ProjectsError = RegistryError;
    type SwitchError = SwitchProjectError;
    type ForgetError = ForgetProjectError;
    type RegisterError = RegisterProjectError;

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

    fn import(&self, path: &str) -> Result<String, String> {
        let guard = self.context.lock().unwrap_or_else(PoisonError::into_inner);
        let context = guard
            .as_ref()
            .ok_or_else(|| "no project is open yet".to_owned())?;
        import_into(&context.journal, path)
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

    fn start_run(&self) -> Result<String, String> {
        let project = self
            .active_project
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match project {
            Some(project) => start_run(&self.binary_path, &project),
            None => Err("no project is open yet".to_owned()),
        }
    }
}

/// Wires every action the screen can take to the active project's journal, run lock and
/// settings, starting on `start`, and runs the terminal interface with it until the operator
/// quits. The journal watch covers every registered project's state, not just the active
/// project's, so a project switched to, or a directory registered, after the screen opened is
/// covered too.
fn drive(
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

/// Reads the file `path` names as text, refusing with the same wording `ktask-rs import`
/// gives for the same problem.
fn read_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))
}

/// Imports the tasks of the file `path` names into `journal`, at the end of the queue, giving
/// what to show for it — the same words `ktask-rs import` itself would print, whether it
/// succeeded or was refused.
fn import_into(journal: &SqliteJournal, path: &str) -> Result<String, String> {
    let json = read_file(path)?;
    match ktask_core::import_tasks(journal, &SystemClock, &json, Placement::End) {
        Ok(import) => {
            let mut buf = Vec::new();
            render::imported(&import, &mut buf)?;
            Ok(String::from_utf8_lossy(&buf).trim_end().to_owned())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Refuses to open the terminal interface when there is no terminal to draw it on.
fn ensure_terminal() -> Result<(), Failure> {
    if io::stdout().is_terminal() {
        return Ok(());
    }
    Err(Failure {
        message: "the terminal interface needs a terminal; \
                  `ktask-rs list` shows the queue without one"
            .to_owned(),
        code: 2,
    })
}

/// Starts `binary_path run --project <project.name>`, detached from this process — its own
/// process group, so neither this process quitting nor its terminal going away stops it —
/// and waits for it to end, which, when it refuses to start at all, is at once. Returns what
/// it printed either way, the same words `ktask-rs run` itself would show, one line per line
/// it wrote — not folded together — so the queue screen can show each on its own line.
fn start_run(binary_path: &Path, project: &Project) -> Result<String, String> {
    let mut child = Command::new(binary_path)
        .arg("run")
        .arg("--project")
        .arg(&project.name)
        .current_dir(&project.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start ktask-rs run: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ktask-rs run has no standard output".to_owned())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "ktask-rs run has no standard error".to_owned())?;
    let out = std::thread::spawn(move || read_all(&mut stdout));
    let err = std::thread::spawn(move || read_all(&mut stderr));
    child
        .wait()
        .map_err(|e| format!("cannot wait for ktask-rs run: {e}"))?;
    Ok(merged_output(out, err))
}

/// Reads `reader` to its end, giving up whatever came through even when it failed partway.
fn read_all(reader: &mut impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// What `out` and `err` — a spawned command's captured standard output and standard error —
/// printed, kept exactly as the lines they were written on, stdout's lines first, then
/// stderr's when both said something.
fn merged_output(out: JoinHandle<Vec<u8>>, err: JoinHandle<Vec<u8>>) -> String {
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).trim_end().to_owned();
    let stdout = text(out.join().unwrap_or_default());
    let stderr = text(err.join().unwrap_or_default());
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}\n{stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => String::new(),
    }
}

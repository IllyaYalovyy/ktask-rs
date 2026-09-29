//! `ktask-rs tui`: open the terminal interface on the project's queue.

use std::cell::RefCell;
use std::io::{self, IsTerminal, Read};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ktask_adapters::{
    FileJournalWatch, FileRunLock, GitCli, SqliteJournal, SqliteRegistry, SystemClock,
    TomlSettingsStore,
};
use ktask_core::{Placement, Project, QueueView, SettingView, TaskDraft, TaskId};

use crate::context::{
    current_exe, journal_file, merge_project, open_registry, open_settings_store, resolve,
    run_lock_file, state_root,
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

/// Opens the terminal interface on the queue of the project selected, or the current one.
pub(crate) fn run(args: &Args, project: Option<&str>) -> Result<(), Failure> {
    ensure_terminal()?;
    let registry = open_registry()?;
    let project = merge_project(project, args.project.as_deref())?;
    let (project, _settings) = resolve(&registry, project.as_deref())?;
    let binary_path = current_exe()?;
    Ok(drive(&registry, project, binary_path)?)
}

/// Everything wired to whichever project is active: its journal, run lock and settings —
/// reopened fresh each time the operator switches to another one, so every action from then on
/// applies to it.
struct ProjectContext {
    project: Project,
    journal: SqliteJournal,
    lock: FileRunLock,
    settings_store: TomlSettingsStore,
}

impl ProjectContext {
    fn open(project: Project) -> Result<Self, String> {
        let journal = SqliteJournal::open(&journal_file(&project)?).map_err(|e| e.to_string())?;
        let lock = FileRunLock::new(run_lock_file(&project)?);
        let settings_store = open_settings_store(&project)?;
        Ok(Self {
            project,
            journal,
            lock,
            settings_store,
        })
    }
}

/// Wires every action the screen can take to the active project's journal, run lock and
/// settings, starting on `project`, and runs the terminal interface with them until the
/// operator quits. The journal watch covers every registered project's state, not just
/// `project`'s, so a project switched to after the screen opened is covered too.
fn drive(registry: &SqliteRegistry, project: Project, binary_path: PathBuf) -> Result<(), String> {
    let context = RefCell::new(ProjectContext::open(project.clone())?);
    let watch = FileJournalWatch::open(&state_root()?, true).map_err(|e| e.to_string())?;
    let active_project = Arc::new(Mutex::new(project));
    let run_project = Arc::clone(&active_project);
    ktask_tui::run(
        actions(&context, &active_project, registry),
        move || {
            let project = run_project
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            start_run(&binary_path, &project)
        },
        watch,
    )
}

/// Every action the screen can take, wired to whichever project `context` and
/// `active_project` currently hold — switching either is `switch_project`'s job, not this
/// function's.
#[allow(clippy::type_complexity)]
fn actions<'a>(
    context: &'a RefCell<ProjectContext>,
    active_project: &'a Mutex<Project>,
    registry: &'a SqliteRegistry,
) -> ktask_tui::Actions<
    impl FnMut(bool) -> Result<QueueView, String> + 'a,
    impl FnMut(TaskId) -> Result<(), String> + 'a,
    impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>> + 'a,
    impl FnMut() -> Result<Vec<SettingView>, String> + 'a,
    impl FnMut(&str, &str) -> Result<SettingView, String> + 'a,
    impl FnMut(&str) -> Result<String, String> + 'a,
    impl FnMut() -> Result<Vec<Project>, String> + 'a,
    impl FnMut(&str) -> Result<QueueView, String> + 'a,
> {
    ktask_tui::Actions {
        load: |show_cancelled| load_queue(context, show_cancelled),
        remove: |id| {
            ktask_core::remove_task(&context.borrow().journal, &SystemClock, id)
                .map_err(|e| e.to_string())
        },
        add: |draft: &TaskDraft, placement: Placement| {
            ktask_core::add_task(&context.borrow().journal, &SystemClock, draft, placement)
                .map(|task| task.id)
                .map_err(|problems| problems.iter().map(ToString::to_string).collect())
        },
        load_settings: || load_settings(&context.borrow().settings_store),
        save_setting: |name: &str, value: &str| save_setting_for(context, name, value),
        import: |path: &str| import_into(&context.borrow().journal, path),
        load_projects: || ktask_core::list_projects(registry).map_err(|e| e.to_string()),
        switch_project: |name: &str| switch_project(context, active_project, registry, name),
    }
}

/// The active project's queue, with the cancelled tasks when `show_cancelled`.
fn load_queue(
    context: &RefCell<ProjectContext>,
    show_cancelled: bool,
) -> Result<QueueView, String> {
    let ctx = context.borrow();
    ktask_core::queue_view(
        ctx.project.clone(),
        &ctx.journal,
        &SystemClock,
        &ctx.lock,
        show_cancelled,
    )
    .map_err(|e| e.to_string())
}

/// Changes the active project's setting `name` to `value`, as the settings screen was
/// submitted.
fn save_setting_for(
    context: &RefCell<ProjectContext>,
    name: &str,
    value: &str,
) -> Result<SettingView, String> {
    let ctx = context.borrow();
    save_setting(&ctx.settings_store, &ctx.project.path, name, value)
}

/// Switches the active project to the registered one called `name`: reopens its journal, run
/// lock and settings fresh, replacing whichever project's they were, and gives its queue —
/// every action after this applies to the new project. Fails, changing nothing, when `name` is
/// not a registered project or its state cannot be opened.
fn switch_project(
    context: &RefCell<ProjectContext>,
    active_project: &Mutex<Project>,
    registry: &SqliteRegistry,
    name: &str,
) -> Result<QueueView, String> {
    let projects = ktask_core::list_projects(registry).map_err(|e| e.to_string())?;
    let project = projects
        .into_iter()
        .find(|project| project.name == name)
        .ok_or_else(|| format!("unknown project {name:?}"))?;
    let fresh = ProjectContext::open(project.clone())?;
    let view = ktask_core::queue_view(
        project.clone(),
        &fresh.journal,
        &SystemClock,
        &fresh.lock,
        false,
    )
    .map_err(|e| e.to_string())?;
    *context.borrow_mut() = fresh;
    *active_project
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = project;
    Ok(view)
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

/// Every project setting, for the settings screen to open on.
fn load_settings(store: &TomlSettingsStore) -> Result<Vec<SettingView>, String> {
    ktask_core::show_settings(store).map_err(|e| e.to_string())
}

/// Changes the setting `name` to `value`, as the settings screen was submitted.
fn save_setting(
    store: &TomlSettingsStore,
    project_dir: &Path,
    name: &str,
    value: &str,
) -> Result<SettingView, String> {
    ktask_core::set_setting(store, &GitCli, project_dir, name, value).map_err(|e| e.to_string())
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

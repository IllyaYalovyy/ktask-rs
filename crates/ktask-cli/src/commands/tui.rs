//! `ktask-rs tui`: open the terminal interface on the project's queue.

use std::io::{self, IsTerminal, Read};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;

use ktask_adapters::{
    FileJournalWatch, FileRunLock, GitCli, SqliteJournal, SystemClock, TomlSettingsStore,
};
use ktask_core::{Project, SettingView};

use crate::context::{
    current_exe, journal_file, open_registry, open_settings_store, resolve, run_lock_file,
};
use crate::error::Failure;

/// `ktask-rs tui`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Opens the terminal interface on the queue of the project selected, or the current one.
pub(crate) fn run(args: &Args) -> Result<(), Failure> {
    ensure_terminal()?;
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, args.project.as_deref())?;
    let path = journal_file(&project)?;
    let journal = SqliteJournal::open(&path).map_err(|e| e.to_string())?;
    let watch = FileJournalWatch::open(&path).map_err(|e| e.to_string())?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    let binary_path = current_exe()?;
    let run_project = project.clone();
    let settings_store = open_settings_store(&project)?;
    let settings_project_dir = project.path.clone();
    Ok(ktask_tui::run(
        |show_cancelled| {
            ktask_core::queue_view(
                project.clone(),
                &journal,
                &SystemClock,
                &lock,
                show_cancelled,
            )
            .map_err(|e| e.to_string())
        },
        |id| ktask_core::remove_task(&journal, &SystemClock, id).map_err(|e| e.to_string()),
        |draft, placement| {
            ktask_core::add_task(&journal, &SystemClock, draft, placement)
                .map(|task| task.id)
                .map_err(|problems| problems.iter().map(ToString::to_string).collect())
        },
        move || start_run(&binary_path, &run_project),
        || load_settings(&settings_store),
        |name, value| save_setting(&settings_store, &settings_project_dir, name, value),
        watch,
    )?)
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
/// it printed either way, the same words `ktask-rs run` itself would show, folded onto one
/// line so it fits the screen's one line for it.
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
    Ok(one_line(out, err))
}

/// Reads `reader` to its end, giving up whatever came through even when it failed partway.
fn read_all(reader: &mut impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// What `out` and `err` — a spawned command's captured standard output and standard error —
/// printed, each folded onto one line, both together when both said something.
fn one_line(out: JoinHandle<Vec<u8>>, err: JoinHandle<Vec<u8>>) -> String {
    let fold = |bytes: Vec<u8>| {
        String::from_utf8_lossy(&bytes)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let stdout = fold(out.join().unwrap_or_default());
    let stderr = fold(err.join().unwrap_or_default());
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout} {stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => String::new(),
    }
}

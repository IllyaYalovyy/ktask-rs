//! The loop that owns the terminal and feeds events into [`update`].

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use ktask_core::{JournalWatch, Placement, Project, QueueView, SettingView, TaskDraft, TaskId};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;

use crate::registration_form::RegistrationForm;
use crate::{App, Event, render, update};

/// What the loop starts on: the project's queue, already resolved, or a name still needed to
/// register the current directory under, because its own folder name is already registered for
/// another path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// The project was resolved already; the loop starts by loading its queue.
    Ready,
    /// The directory could not be resolved to a project: `message` is the refusal that says
    /// why, the same words `ktask-rs` gives for the same conflict, and the loop starts by
    /// asking for a name to register the directory under instead.
    NameTaken {
        /// The refusal to show above the name field.
        message: String,
    },
}

/// Something the loop is woken by: a key (or resize) at the terminal, the journal having
/// changed under it, the process being told to stop, a run this screen started ending or
/// refusing to start, or — only while a task is running — the tick that keeps its elapsed
/// time moving.
enum Wake {
    /// An input arrived at the terminal.
    Input(Input),
    /// The journal changed; the queue is stale and worth loading again.
    Changed,
    /// SIGTERM or SIGHUP arrived — the terminal went away, or the operator or the system
    /// asked the process to stop. There is nothing to weigh against a form's content here:
    /// the terminal is leaving regardless, so the loop ends at once.
    Stop,
    /// No other wake arrived before [`TICK`] passed, while a task was running: the queue is
    /// loaded again so the running task's elapsed time moves even though nothing else changed.
    Tick,
    /// The run this screen started has ended, or could not start at all, printing this — the
    /// same words `ktask-rs run` itself would show, one line per line it wrote.
    RunMessage(String),
}

/// Every callback the loop uses to read or change state outside the terminal, bundled into
/// one value so [`run`] and [`drive`] take few enough arguments for Clippy's limit on them.
pub struct Actions<
    Load,
    Remove,
    Add,
    LoadSettings,
    SaveSetting,
    Import,
    LoadProjects,
    SwitchProject,
    ForgetProject,
    Register,
> {
    /// Fetches the queue to show, with the cancelled tasks when told to.
    pub load: Load,
    /// Removes a task the operator confirmed removing.
    pub remove: Remove,
    /// Adds the task the operator wrote in the form, where the form says.
    pub add: Add,
    /// The project's settings, for the settings screen to open on.
    pub load_settings: LoadSettings,
    /// Changes the setting named by the settings screen's focused field to the value it was
    /// submitted with; the new setting, or why it was refused.
    pub save_setting: SaveSetting,
    /// Imports the tasks of the file whose path the operator wrote in the import form,
    /// giving what to show for it — the same words `ktask-rs import` itself would print,
    /// whether it succeeded or was refused.
    pub import: Import,
    /// The registered projects, for the project picker to open on — the same list
    /// `ktask-rs project list` prints.
    pub load_projects: LoadProjects,
    /// Switches to the project the picker was submitted with, so every action from here on
    /// applies to it, giving its fresh queue, or why the switch did not happen.
    pub switch_project: SwitchProject,
    /// Forgets the project the picker's confirmation named, giving the registered projects
    /// that remain — the same list `ktask-rs project list` prints afterward — or why nothing
    /// was forgotten.
    pub forget_project: ForgetProject,
    /// Registers the current directory under the name the registration screen was submitted
    /// with, giving its fresh queue on success — the same outcome `ktask-rs project register
    /// --name` gives for the same name — or why nothing was registered, for the screen to show
    /// and ask again.
    pub register: Register,
}

/// Closures carry no useful debug representation of their own; this names the type without
/// them, which is all `#[derive(Debug)]` could offer here in any case.
impl<
    Load,
    Remove,
    Add,
    LoadSettings,
    SaveSetting,
    Import,
    LoadProjects,
    SwitchProject,
    ForgetProject,
    Register,
> std::fmt::Debug
    for Actions<
        Load,
        Remove,
        Add,
        LoadSettings,
        SaveSetting,
        Import,
        LoadProjects,
        SwitchProject,
        ForgetProject,
        Register,
    >
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Actions").finish_non_exhaustive()
    }
}

/// How often the loop wakes on its own to refresh a running task's elapsed time, while one is
/// running. Nothing wakes it on a timer otherwise.
const TICK: Duration = Duration::from_secs(1);

/// How long a burst of [`Wake::Changed`] is given to settle before it is read: a task's
/// attempt appends several events close together — its status, then each step's own outcome —
/// so reading right on the first of them risks a reload caught between two of a related
/// group, showing one without the other. Short enough that a human never notices the delay.
const SETTLE: Duration = Duration::from_millis(20);

/// Starts a terminal synchronized update: a reader that stops at the matching end marker never
/// sees a frame half drawn.
const SYNC_START: &[u8] = b"\x1b[?2026h";
/// Ends a terminal synchronized update.
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Draws one frame of `app`, wrapped in the terminal's synchronized-update markers.
fn draw(terminal: &mut DefaultTerminal, app: &App) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot draw the screen: {e}");
    terminal.backend_mut().write_all(SYNC_START).map_err(fail)?;
    terminal
        .draw(|frame| {
            if let Some(cursor) = render(app, frame.area(), frame.buffer_mut()) {
                frame.set_cursor_position(cursor);
            }
        })
        .map_err(fail)?;
    let backend = terminal.backend_mut();
    backend.write_all(SYNC_END).map_err(fail)?;
    backend.flush().map_err(fail)
}

/// Runs the terminal interface until the operator quits.
///
/// `actions.load` fetches the queue to show, with the cancelled tasks when it is told to. It
/// is called at the start, again whenever `watch` reports the journal changed, and when the
/// operator asks for cancelled tasks or stops asking. `actions.remove` removes a task the
/// operator confirmed removing, after which the queue is loaded again. `actions.add` adds the
/// task the operator wrote in the form where the form says, and gives its number, or the
/// reasons it was not added when it was not. `actions.import` imports the tasks of the file
/// whose path the operator wrote in the import form, giving what to show for it — the same
/// words `ktask-rs import` itself would print — shown on the screen once it returns.
/// `start_run` starts executing the pending tasks, exactly as `ktask-rs run` does, and is
/// called on a thread of its own each time the operator asks for it, so the screen stays
/// responsive for however long the run takes; it blocks until the run it starts ends, or
/// refuses to start at all, and returns what it printed either way — the same words
/// `ktask-rs run` itself would show — shown on the screen once it returns, one line per line
/// it wrote. `actions.load_settings`
/// fetches every project setting, called each time the operator opens the settings screen.
/// `actions.save_setting` changes the setting named by the settings screen's focused field to
/// the value it was submitted with, giving the new setting, or the reasons it was refused —
/// an unknown setting or an invalid value — shown on the screen instead of closing it.
/// `actions.load_projects` fetches the registered projects, called each time the operator
/// opens the project picker. `actions.switch_project` switches to the project the picker was
/// submitted with, so every action from here on applies to it, giving its fresh queue, or why
/// the switch did not happen — shown on the screen instead of closing it. `watch`
/// blocks until the journal changes; it is polled from a dedicated thread, so a task added,
/// inserted or removed by another process — a run included — shows in the next frame without
/// the loop itself ever waking on a timer — except while a task is running, when the queue is
/// loaded again on a short timer too, so the running task's elapsed time keeps moving even
/// though nothing else changed; the loop goes back to waiting with no timer once nothing is
/// running. The terminal is put back as it was on every way out: the operator quitting,
/// SIGTERM or SIGHUP (its terminal going away sends this), or an error — a run `start_run`
/// started keeps going regardless, since it does not depend on this process to finish.
///
/// # Errors
///
/// Fails when the queue or the settings cannot be loaded, a task cannot be removed or the
/// terminal cannot be used. `start` chooses what the loop shows first: `Start::Ready` loads the
/// queue with `actions.load` as described above; `Start::NameTaken` opens the registration
/// screen instead, showing its message, and `actions.load` is not called until a name typed
/// there is submitted and `actions.register` succeeds with it, exactly as `switch_project`
/// replaces the queue on show when the picker is submitted.
pub fn run(
    start: Start,
    actions: Actions<
        impl FnMut(bool) -> Result<QueueView, String>,
        impl FnMut(TaskId) -> Result<(), String>,
        impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
        impl FnMut() -> Result<Vec<SettingView>, String>,
        impl FnMut(&str, &str) -> Result<SettingView, String>,
        impl FnMut(&str) -> Result<String, String>,
        impl FnMut() -> Result<Vec<Project>, String>,
        impl FnMut(&str) -> Result<QueueView, String>,
        impl FnMut(&str) -> Result<Vec<Project>, String>,
        impl FnMut(&str) -> Result<QueueView, String>,
    >,
    start_run: impl Fn() -> Result<String, String> + Send + Sync + 'static,
    watch: impl JournalWatch + Send + 'static,
) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let start_run = Arc::new(start_run);
    let result = spawn_wakes(watch).and_then(|(sender, wakes)| {
        drive(start, &mut terminal, actions, &start_run, &sender, &wakes)
    });
    ratatui::restore();
    result
}

/// Starts the threads that turn keyboard input, journal changes and a termination signal into
/// a single stream the loop can block on, with no timer of its own; returns the sender they
/// share too, so [`drive`] can raise [`Wake::RunMessage`] from the thread it starts for
/// `start_run` the same way.
///
/// # Errors
///
/// Fails when SIGTERM and SIGHUP cannot be watched for.
fn spawn_wakes(
    watch: impl JournalWatch + Send + 'static,
) -> Result<(Sender<Wake>, Receiver<Wake>), String> {
    let (sender, receiver) = mpsc::channel();
    let keys = sender.clone();
    thread::spawn(move || {
        while let Ok(input) = event::read() {
            if keys.send(Wake::Input(input)).is_err() {
                return;
            }
        }
    });
    let changed = sender.clone();
    thread::spawn(move || {
        while watch.wait().is_ok() {
            if changed.send(Wake::Changed).is_err() {
                return;
            }
        }
    });
    let mut signals = Signals::new([SIGTERM, SIGHUP])
        .map_err(|e| format!("cannot watch for a termination signal: {e}"))?;
    let stop = sender.clone();
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = stop.send(Wake::Stop);
        }
    });
    Ok((sender, receiver))
}

/// The next [`Wake`] to act on: waits at most [`TICK`] while a task is running, so its
/// elapsed time keeps moving even when nothing else wakes the loop, or indefinitely
/// otherwise.
fn next_wake(wakes: &Receiver<Wake>, running: bool) -> Result<Wake, String> {
    if !running {
        return wakes
            .recv()
            .map_err(|_| "the keyboard and journal-watch threads both stopped".to_owned());
    }
    match wakes.recv_timeout(TICK) {
        Ok(wake) => Ok(wake),
        Err(RecvTimeoutError::Timeout) => Ok(Wake::Tick),
        Err(RecvTimeoutError::Disconnected) => {
            Err("the keyboard and journal-watch threads both stopped".to_owned())
        }
    }
}

/// The wake `drive`'s loop acts on next: one already drained ahead of its turn while
/// collapsing the previous burst, taken from `pending`, or [`next_wake`] otherwise.
fn next(pending: &mut Option<Wake>, wakes: &Receiver<Wake>, running: bool) -> Result<Wake, String> {
    match pending.take() {
        Some(wake) => Ok(wake),
        None => next_wake(wakes, running),
    }
}

/// Waits out [`SETTLE`] for a burst of [`Wake::Changed`] to finish landing, then drains every
/// one of it and every [`Wake::Tick`] queued alongside it, so the burst collapses into the one
/// reload it warrants, read whole rather than caught between two commits of a group appended
/// close together. Returns the first wake of a different kind found while draining, for the
/// next iteration to handle in its turn — never dropped, just deferred.
fn drain_changes(wakes: &Receiver<Wake>) -> Option<Wake> {
    thread::sleep(SETTLE);
    loop {
        match wakes.try_recv() {
            Ok(Wake::Changed | Wake::Tick) => {}
            Ok(other) => return Some(other),
            Err(_) => return None,
        }
    }
}

/// The app after `input`, carrying out whatever it left pending for the loop — removing a
/// task, adding one, loading or saving a setting, or starting a run — and whether the queue
/// is worth loading again afterward: not, only, when nothing pending was found and the
/// cancelled-tasks switch did not change either, since a plain cursor move changes nothing
/// the journal knows about.
/// Removes or adds the task `app` has pending, if either: `(app, true)` when one was, `(app,
/// false)`, unchanged, otherwise.
fn handle_task_action<Remove, Add>(
    mut app: App,
    remove: &mut Remove,
    add: &mut Add,
) -> Result<(App, bool), String>
where
    Remove: FnMut(TaskId) -> Result<(), String>,
    Add: FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
{
    if let Some(id) = app.removal.take() {
        remove(id)?;
        return Ok((app, true));
    }
    if let Some((draft, placement)) = app.submission.take() {
        let added = match add(&draft, placement) {
            Ok(id) => Event::Added(id),
            Err(problems) => Event::Rejected(problems),
        };
        return Ok((update(app, added), true));
    }
    Ok((app, false))
}

/// Loads the settings screen or saves the setting `app` has pending, if either: `(app, true)`
/// when one was, `(app, false)`, unchanged, otherwise.
fn handle_settings_action<LoadSettings, SaveSetting>(
    mut app: App,
    load_settings: &mut LoadSettings,
    save_setting: &mut SaveSetting,
) -> Result<(App, bool), String>
where
    LoadSettings: FnMut() -> Result<Vec<SettingView>, String>,
    SaveSetting: FnMut(&str, &str) -> Result<SettingView, String>,
{
    if app.settings_requested.take().is_some() {
        let views = load_settings()?;
        return Ok((update(app, Event::SettingsLoaded(views)), true));
    }
    if let Some((name, value)) = app.setting_submission.take() {
        let event = match save_setting(name, &value) {
            Ok(_) => Event::SettingSaved,
            Err(message) => Event::SettingRejected(message),
        };
        return Ok((update(app, event), true));
    }
    Ok((app, false))
}

/// Loads the project picker, switches to the project `app` has pending, or forgets the one its
/// confirmation named, whichever `app` has pending: `(app, true)` when one was, `(app, false)`,
/// unchanged, otherwise.
fn handle_projects_action<LoadProjects, SwitchProject, ForgetProject>(
    mut app: App,
    load_projects: &mut LoadProjects,
    switch_project: &mut SwitchProject,
    forget_project: &mut ForgetProject,
) -> Result<(App, bool), String>
where
    LoadProjects: FnMut() -> Result<Vec<Project>, String>,
    SwitchProject: FnMut(&str) -> Result<QueueView, String>,
    ForgetProject: FnMut(&str) -> Result<Vec<Project>, String>,
{
    if app.projects_requested.take().is_some() {
        let projects = load_projects()?;
        return Ok((update(app, Event::ProjectsLoaded(projects)), true));
    }
    if let Some(name) = app.project_switch.take() {
        let event = match switch_project(&name) {
            Ok(queue) => Event::ProjectSwitched(queue),
            Err(message) => Event::ProjectSwitchFailed(message),
        };
        return Ok((update(app, event), true));
    }
    if let Some(name) = app.project_forget.take() {
        let event = match forget_project(&name) {
            Ok(projects) => Event::ProjectForgotten(projects),
            Err(message) => Event::ProjectForgetFailed(message),
        };
        return Ok((update(app, event), true));
    }
    Ok((app, false))
}

/// Registers the current directory under the name `app`'s registration screen was submitted
/// with, if any. Succeeding gives the fresh queue to show, exactly as if it had been loaded
/// from the start, so `(app, true)`, the same as every other background action, tells the loop
/// to load it. Failing keeps the screen open and shows why, the same words `ktask-rs project
/// register --name` gives for the same conflict, so another name can be tried — `(app, false)`
/// here, unlike every other background action's own failure, since there is no project open yet
/// for the loop to load a queue from.
fn handle_registration_action<Register>(mut app: App, register: &mut Register) -> (App, bool)
where
    Register: FnMut(&str) -> Result<QueueView, String>,
{
    let Some(name) = app.registration_submission.take() else {
        return (app, false);
    };
    match register(&name) {
        Ok(queue) => (update(app, Event::Registered(queue)), true),
        Err(message) => (update(app, Event::RegistrationFailed(message)), false),
    }
}

/// Imports the file `app` has pending, if any: `(app, true)` when one was, `(app, false)`,
/// unchanged, otherwise. Shows the same result, or the same refusal, `ktask-rs import` itself
/// would print, one line per line of it.
fn handle_import_action<Import>(mut app: App, import: &mut Import) -> (App, bool)
where
    Import: FnMut(&str) -> Result<String, String>,
{
    let Some(path) = app.import_submission.take() else {
        return (app, false);
    };
    let text = match import(&path) {
        Ok(text) | Err(text) => text,
    };
    (update(app, Event::ImportMessage(text)), true)
}

/// Evaluates to the app a `(App, bool)` pair carries once its `bool` is read, returning at
/// once with `Ok((app, true))` when it was `true` — the shared shape of trying one background
/// action after another in [`handle_input`], in the order the first one pending wins.
macro_rules! or_return_handled {
    ($pair:expr) => {{
        let (app, handled) = $pair;
        if handled {
            return Ok((app, true));
        }
        app
    }};
}

/// Tries registering the current directory, removing or adding a task, loading or saving a
/// setting, and loading, switching or forgetting a project, in that order: the first one `app`
/// has pending wins. `(app, true)` when one did, `(app, false)` otherwise.
fn try_background_actions<L, R, A, LS, SS, I, LP, SP, FP, Rg>(
    app: App,
    actions: &mut Actions<L, R, A, LS, SS, I, LP, SP, FP, Rg>,
) -> Result<(App, bool), String>
where
    R: FnMut(TaskId) -> Result<(), String>,
    A: FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
    LS: FnMut() -> Result<Vec<SettingView>, String>,
    SS: FnMut(&str, &str) -> Result<SettingView, String>,
    LP: FnMut() -> Result<Vec<Project>, String>,
    SP: FnMut(&str) -> Result<QueueView, String>,
    FP: FnMut(&str) -> Result<Vec<Project>, String>,
    Rg: FnMut(&str) -> Result<QueueView, String>,
{
    let app = or_return_handled!(handle_registration_action(app, &mut actions.register));
    let app = or_return_handled!(handle_task_action(
        app,
        &mut actions.remove,
        &mut actions.add
    )?);
    let app = or_return_handled!(handle_settings_action(
        app,
        &mut actions.load_settings,
        &mut actions.save_setting,
    )?);
    handle_projects_action(
        app,
        &mut actions.load_projects,
        &mut actions.switch_project,
        &mut actions.forget_project,
    )
}

fn handle_input<L, R, A, LS, SS, I, LP, SP, FP, Rg>(
    mut app: App,
    actions: &mut Actions<L, R, A, LS, SS, I, LP, SP, FP, Rg>,
    input: &Input,
    start_run: &Arc<impl Fn() -> Result<String, String> + Send + Sync + 'static>,
    sender: &Sender<Wake>,
) -> Result<(App, bool), String>
where
    R: FnMut(TaskId) -> Result<(), String>,
    A: FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
    LS: FnMut() -> Result<Vec<SettingView>, String>,
    SS: FnMut(&str, &str) -> Result<SettingView, String>,
    I: FnMut(&str) -> Result<String, String>,
    LP: FnMut() -> Result<Vec<Project>, String>,
    SP: FnMut(&str) -> Result<QueueView, String>,
    FP: FnMut(&str) -> Result<Vec<Project>, String>,
    Rg: FnMut(&str) -> Result<QueueView, String>,
{
    let asked = app.show_cancelled;
    if let Some(event) = translate(input) {
        app = update(app, event);
    }
    let app = or_return_handled!(try_background_actions(app, actions)?);
    let mut app = or_return_handled!(handle_import_action(app, &mut actions.import));
    if app.run_requested.take().is_some() {
        spawn_run(Arc::clone(start_run), sender.clone());
        return Ok((app, true));
    }
    let should_reload = app.show_cancelled != asked;
    Ok((app, should_reload))
}

/// Whether `app`'s queue shows a task currently running.
fn task_running(app: &App) -> bool {
    app.queue
        .as_ref()
        .is_some_and(|queue| queue.summary.running > 0)
}

/// What handling one `wake` produces: `None` when the loop should stop; otherwise `app` to
/// carry on with, a wake drained ahead of its turn while collapsing a burst of
/// [`Wake::Changed`] for the next call to receive first, and whether the queue is worth
/// loading again before the next frame.
fn step<L, R, A, LS, SS, I, LP, SP, FP, Rg>(
    app: App,
    wake: Wake,
    wakes: &Receiver<Wake>,
    actions: &mut Actions<L, R, A, LS, SS, I, LP, SP, FP, Rg>,
    start_run: &Arc<impl Fn() -> Result<String, String> + Send + Sync + 'static>,
    sender: &Sender<Wake>,
) -> Result<Option<(App, Option<Wake>, bool)>, String>
where
    R: FnMut(TaskId) -> Result<(), String>,
    A: FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
    LS: FnMut() -> Result<Vec<SettingView>, String>,
    SS: FnMut(&str, &str) -> Result<SettingView, String>,
    I: FnMut(&str) -> Result<String, String>,
    LP: FnMut() -> Result<Vec<Project>, String>,
    SP: FnMut(&str) -> Result<QueueView, String>,
    FP: FnMut(&str) -> Result<Vec<Project>, String>,
    Rg: FnMut(&str) -> Result<QueueView, String>,
{
    match wake {
        Wake::Stop => Ok(None),
        // A single underlying change can be reported as a burst of several — a recursive
        // watch sees a project's own rollback-journal file appear and disappear around each
        // commit, on top of the change itself. Collapsing a burst into the one reload it
        // warrants keeps a run from being slowed down by a reload racing every one of its own
        // writes for the journal's lock. Anything drained that is not itself part of the
        // burst is kept, not lost, for the next call.
        Wake::Changed => Ok(Some((app, drain_changes(wakes), true))),
        Wake::Tick => Ok(Some((app, None, true))),
        Wake::RunMessage(text) => Ok(Some((update(app, Event::RunMessage(text)), None, true))),
        Wake::Input(input) => {
            let (app, reload) = handle_input(app, actions, &input, start_run, sender)?;
            Ok(Some((app, None, reload)))
        }
    }
}

/// The app the loop starts on: the queue `load` fetches, or the registration screen open on
/// `start`'s message, when it says a name is needed first.
fn initial_app(
    start: Start,
    load: &mut impl FnMut(bool) -> Result<QueueView, String>,
) -> Result<App, String> {
    Ok(match start {
        Start::Ready => update(App::default(), Event::Loaded(load(false)?)),
        Start::NameTaken { message } => App {
            registration: Some(RegistrationForm::new(message)),
            ..App::default()
        },
    })
}

fn drive(
    start: Start,
    terminal: &mut DefaultTerminal,
    mut actions: Actions<
        impl FnMut(bool) -> Result<QueueView, String>,
        impl FnMut(TaskId) -> Result<(), String>,
        impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
        impl FnMut() -> Result<Vec<SettingView>, String>,
        impl FnMut(&str, &str) -> Result<SettingView, String>,
        impl FnMut(&str) -> Result<String, String>,
        impl FnMut() -> Result<Vec<Project>, String>,
        impl FnMut(&str) -> Result<QueueView, String>,
        impl FnMut(&str) -> Result<Vec<Project>, String>,
        impl FnMut(&str) -> Result<QueueView, String>,
    >,
    start_run: &Arc<impl Fn() -> Result<String, String> + Send + Sync + 'static>,
    sender: &Sender<Wake>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let app = initial_app(start, &mut actions.load)?;
    run_loop(app, terminal, actions, start_run, sender, wakes)
}

/// Draws `app`, then feeds it whatever wakes the loop until the operator quits or a fatal error
/// occurs, reloading the queue whenever handling a wake calls for it. A wake drained ahead of
/// its turn while collapsing a burst of [`Wake::Changed`] is carried in `pending`, so the next
/// iteration handles it rather than losing it.
fn run_loop(
    mut app: App,
    terminal: &mut DefaultTerminal,
    mut actions: Actions<
        impl FnMut(bool) -> Result<QueueView, String>,
        impl FnMut(TaskId) -> Result<(), String>,
        impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
        impl FnMut() -> Result<Vec<SettingView>, String>,
        impl FnMut(&str, &str) -> Result<SettingView, String>,
        impl FnMut(&str) -> Result<String, String>,
        impl FnMut() -> Result<Vec<Project>, String>,
        impl FnMut(&str) -> Result<QueueView, String>,
        impl FnMut(&str) -> Result<Vec<Project>, String>,
        impl FnMut(&str) -> Result<QueueView, String>,
    >,
    start_run: &Arc<impl Fn() -> Result<String, String> + Send + Sync + 'static>,
    sender: &Sender<Wake>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let mut pending: Option<Wake> = None;
    loop {
        draw(terminal, &app)?;
        if app.quit {
            return Ok(());
        }
        let wake = next(&mut pending, wakes, task_running(&app))?;
        let Some((new_app, new_pending, reload)) =
            step(app, wake, wakes, &mut actions, start_run, sender)?
        else {
            return Ok(());
        };
        pending = new_pending;
        app = if reload {
            let queue = (actions.load)(new_app.show_cancelled)?;
            update(new_app, Event::Loaded(queue))
        } else {
            new_app
        };
    }
}

/// Calls `start_run` on a thread of its own, so the loop stays responsive for however long
/// the run it starts takes, and raises [`Wake::RunMessage`] with what it printed once it
/// returns, whether it ran to completion, stopped partway, or refused to start at all.
fn spawn_run(
    start_run: Arc<impl Fn() -> Result<String, String> + Send + Sync + 'static>,
    sender: Sender<Wake>,
) {
    thread::spawn(move || {
        let text = match start_run() {
            Ok(text) | Err(text) => text,
        };
        let _ = sender.send(Wake::RunMessage(text));
    });
}

/// The event an input from the terminal means, if it means any.
fn translate(input: &Input) -> Option<Event> {
    match input {
        Input::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char(letter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Event::Ctrl(letter))
            }
            code => Some(Event::Key(code)),
        },
        Input::Resize(..) => Some(Event::Resize),
        _ => None,
    }
}

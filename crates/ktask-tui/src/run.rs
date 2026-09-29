//! The loop that owns the terminal and feeds events into [`update`].

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use ktask_core::{JournalWatch, Placement, QueueView, SettingView, TaskDraft, TaskId};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;

use crate::{App, Event, render, update};

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
    /// same words `ktask-rs run` itself would show.
    RunMessage(String),
}

/// Every callback the loop uses to read or change state outside the terminal, bundled into
/// one value so [`drive`] takes few enough arguments for Clippy's limit on them.
struct Actions<Load, Remove, Add, LoadSettings, SaveSetting> {
    /// Fetches the queue to show, with the cancelled tasks when told to.
    load: Load,
    /// Removes a task the operator confirmed removing.
    remove: Remove,
    /// Adds the task the operator wrote in the form, where the form says.
    add: Add,
    /// The project's settings, for the settings screen to open on.
    load_settings: LoadSettings,
    /// Changes the setting named by the settings screen's focused field to the value it was
    /// submitted with; the new setting, or why it was refused.
    save_setting: SaveSetting,
}

/// How often the loop wakes on its own to refresh a running task's elapsed time, while one is
/// running. Nothing wakes it on a timer otherwise.
const TICK: Duration = Duration::from_secs(1);

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
/// `load` fetches the queue to show, with the cancelled tasks when it is told to. It is called
/// at the start, again whenever `watch` reports the journal changed, and when the operator asks
/// for cancelled tasks or stops asking. `remove` removes a task the operator confirmed removing,
/// after which the queue is loaded again. `add` adds the task the operator wrote in the form
/// where the form says, and gives its number, or the reasons it was not added when it was not.
/// `start_run` starts executing the pending tasks, exactly as `ktask-rs run` does, and is
/// called on a thread of its own each time the operator asks for it, so the screen stays
/// responsive for however long the run takes; it blocks until the run it starts ends, or
/// refuses to start at all, and returns what it printed either way — the same words
/// `ktask-rs run` itself would show, shown on the screen once it returns. `load_settings`
/// fetches every project setting, called each time the operator opens the settings screen.
/// `save_setting` changes the setting named by the settings screen's focused field to the
/// value it was submitted with, giving the new setting, or the reasons it was refused — an
/// unknown setting or an invalid value — shown on the screen instead of closing it. `watch` blocks
/// until the journal changes; it is polled from a dedicated thread, so a task added, inserted
/// or removed by another process — a run included — shows in the next frame without the loop
/// itself ever waking on a timer — except while a task is running, when the queue is loaded
/// again on a short timer too, so the running task's elapsed time keeps moving even though
/// nothing else changed; the loop goes back to waiting with no timer once nothing is running.
/// The terminal is put back as it was on every way out: the operator quitting, SIGTERM or
/// SIGHUP (its terminal going away sends this), or an error — a run `start_run` started keeps
/// going regardless, since it does not depend on this process to finish.
///
/// # Errors
///
/// Fails when the queue or the settings cannot be loaded, a task cannot be removed or the
/// terminal cannot be used.
pub fn run(
    load: impl FnMut(bool) -> Result<QueueView, String>,
    remove: impl FnMut(TaskId) -> Result<(), String>,
    add: impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
    start_run: impl Fn() -> Result<String, String> + Send + Sync + 'static,
    load_settings: impl FnMut() -> Result<Vec<SettingView>, String>,
    save_setting: impl FnMut(&str, &str) -> Result<SettingView, String>,
    watch: impl JournalWatch + Send + 'static,
) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let start_run = Arc::new(start_run);
    let actions = Actions {
        load,
        remove,
        add,
        load_settings,
        save_setting,
    };
    let result = spawn_wakes(watch)
        .and_then(|(sender, wakes)| drive(&mut terminal, actions, &start_run, &sender, &wakes));
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

fn drive(
    terminal: &mut DefaultTerminal,
    mut actions: Actions<
        impl FnMut(bool) -> Result<QueueView, String>,
        impl FnMut(TaskId) -> Result<(), String>,
        impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
        impl FnMut() -> Result<Vec<SettingView>, String>,
        impl FnMut(&str, &str) -> Result<SettingView, String>,
    >,
    start_run: &Arc<impl Fn() -> Result<String, String> + Send + Sync + 'static>,
    sender: &Sender<Wake>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let mut app = update(App::default(), Event::Loaded((actions.load)(false)?));
    loop {
        draw(terminal, &app)?;
        if app.quit {
            return Ok(());
        }
        let running = app
            .queue
            .as_ref()
            .is_some_and(|queue| queue.summary.running > 0);
        let wake = if running {
            match wakes.recv_timeout(TICK) {
                Ok(wake) => wake,
                Err(RecvTimeoutError::Timeout) => Wake::Tick,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the keyboard and journal-watch threads both stopped".to_owned());
                }
            }
        } else {
            wakes
                .recv()
                .map_err(|_| "the keyboard and journal-watch threads both stopped".to_owned())?
        };
        match wake {
            Wake::Stop => return Ok(()),
            Wake::Changed | Wake::Tick => {}
            Wake::RunMessage(text) => {
                app = update(app, Event::RunMessage(text));
            }
            Wake::Input(input) => {
                let asked = app.show_cancelled;
                if let Some(event) = translate(&input) {
                    app = update(app, event);
                }
                if let Some(id) = app.removal.take() {
                    (actions.remove)(id)?;
                } else if let Some((draft, placement)) = app.submission.take() {
                    let added = match (actions.add)(&draft, placement) {
                        Ok(id) => Event::Added(id),
                        Err(problems) => Event::Rejected(problems),
                    };
                    app = update(app, added);
                } else if app.settings_requested.take().is_some() {
                    let views = (actions.load_settings)()?;
                    app = update(app, Event::SettingsLoaded(views));
                } else if let Some((name, value)) = app.setting_submission.take() {
                    let event = match (actions.save_setting)(name, &value) {
                        Ok(_) => Event::SettingSaved,
                        Err(message) => Event::SettingRejected(message),
                    };
                    app = update(app, event);
                } else if app.run_requested.take().is_some() {
                    spawn_run(Arc::clone(start_run), sender.clone());
                } else if app.show_cancelled == asked {
                    continue;
                }
            }
        }
        let queue = (actions.load)(app.show_cancelled)?;
        app = update(app, Event::Loaded(queue));
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

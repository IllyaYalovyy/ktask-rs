//! The loop that owns the terminal and feeds events into [`update`].

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use ktask_core::{JournalWatch, QueueView};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;

use crate::application::Application;
use crate::registration_screen::RegistrationScreen;
use crate::{App, Event, render, update};

mod actions;
mod report_text;

use actions::handle_input;

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
/// `application` is the one value every use case the screen can reach is called through — see
/// [`Application`]. Its `load_queue` is called at the start, again whenever `watch` reports the
/// journal changed, and when the operator asks for cancelled tasks or stops asking; a failure
/// ends the loop, its message the use case's own, read here where it is about to be shown.
/// `remove_task` removes a task the operator confirmed removing, after which the queue is
/// loaded again. `add_task` adds the task the operator wrote in the form where the form says,
/// and gives its number, or the reasons it was not added when it was not — shown on the form
/// instead of closing it. `import` imports the tasks of the file whose path the operator wrote
/// in the import form, giving what to show for it — the same words `ktask-rs import` itself
/// would print — shown on the screen once it returns. `start_run` starts executing the
/// pending tasks, exactly as `ktask-rs run` does, and is called on a thread of its own each
/// time the operator asks for it, so the screen stays responsive for however long the run
/// takes; it blocks until the run it starts ends, or refuses to start at all, and gives what
/// it printed either way — the same words `ktask-rs run` itself would show — shown on the
/// screen once it returns, one line per line it wrote. `load_settings` fetches every project
/// setting, called each time the operator opens the settings screen. `save_setting` changes
/// the setting named by the settings screen's focused field to the value it was submitted
/// with, giving the new setting, or the reasons it was refused — an unknown setting or an
/// invalid value — shown on the screen instead of closing it. `load_projects` fetches the
/// registered projects, called each time the operator opens the project picker.
/// `switch_project` switches to the project the picker was submitted with, so every action
/// from then on applies to it, giving its fresh queue, or why the switch did not happen —
/// shown on the screen instead of closing it. `watch` blocks until the journal changes; it is
/// polled from a dedicated thread, so a task added, inserted or removed by another process — a
/// run included — shows in the next frame without the loop itself ever waking on a timer —
/// except while a task is running, when the queue is loaded again on a short timer too, so the
/// running task's elapsed time keeps moving even though nothing else changed; the loop goes
/// back to waiting with no timer once nothing is running. The terminal is put back as it was
/// on every way out: the operator quitting, SIGTERM or SIGHUP (its terminal going away sends
/// this), or an error — a run `start_run` started keeps going regardless, since it does not
/// depend on this process to finish.
///
/// # Errors
///
/// Fails when the queue or the settings cannot be loaded, a task cannot be removed or the
/// terminal cannot be used. `start` chooses what the loop shows first: `Start::Ready` loads the
/// queue with `application.load_queue` as described above; `Start::NameTaken` opens the
/// registration screen instead, showing its message, and `load_queue` is not called until a
/// name typed there is submitted and `application.register` succeeds with it, exactly as
/// `switch_project` replaces the queue on show when the picker is submitted.
pub fn run(
    start: Start,
    application: impl Application + Send + Sync + 'static,
    watch: impl JournalWatch + Send + 'static,
) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let application = Arc::new(application);
    let result = spawn_wakes(watch)
        .and_then(|(sender, wakes)| drive(start, &mut terminal, &application, &sender, &wakes));
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

/// The queue to show, with the cancelled tasks when `show_cancelled`, read through
/// `application` and turned into a message, here, where it is about to leave the loop, if it
/// could not be loaded at all.
fn load(application: &impl Application, show_cancelled: bool) -> Result<QueueView, String> {
    application
        .load_queue(show_cancelled)
        .map_err(|error| error.to_string())
}

/// Whether `app`'s queue shows a task currently running.
fn task_running(app: &App) -> bool {
    app.queue
        .view()
        .is_some_and(|queue| queue.summary.running > 0)
}

/// What handling one `wake` produces: `None` when the loop should stop; otherwise `app` to
/// carry on with, a wake drained ahead of its turn while collapsing a burst of
/// [`Wake::Changed`] for the next call to receive first, and whether the queue is worth
/// loading again before the next frame.
fn step<A: Application + Send + Sync + 'static>(
    app: App,
    wake: Wake,
    wakes: &Receiver<Wake>,
    application: &Arc<A>,
    sender: &Sender<Wake>,
) -> Result<Option<(App, Option<Wake>, bool)>, String> {
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
            let (app, reload) = handle_input(app, application, &input, sender)?;
            Ok(Some((app, None, reload)))
        }
    }
}

/// The app the loop starts on: the queue `application.load_queue` fetches, or the
/// registration screen open on `start`'s message, when it says a name is needed first.
fn initial_app(start: Start, application: &impl Application) -> Result<App, String> {
    Ok(match start {
        Start::Ready => update(App::default(), Event::Loaded(load(application, false)?)),
        Start::NameTaken { message } => App {
            registration: Some(RegistrationScreen::new(message)),
            ..App::default()
        },
    })
}

fn drive<A: Application + Send + Sync + 'static>(
    start: Start,
    terminal: &mut DefaultTerminal,
    application: &Arc<A>,
    sender: &Sender<Wake>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let app = initial_app(start, application.as_ref())?;
    run_loop(app, terminal, application, sender, wakes)
}

/// Draws `app`, then feeds it whatever wakes the loop until the operator quits or a fatal error
/// occurs, reloading the queue whenever handling a wake calls for it. A wake drained ahead of
/// its turn while collapsing a burst of [`Wake::Changed`] is carried in `pending`, so the next
/// iteration handles it rather than losing it.
fn run_loop<A: Application + Send + Sync + 'static>(
    mut app: App,
    terminal: &mut DefaultTerminal,
    application: &Arc<A>,
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
        let Some((new_app, new_pending, reload)) = step(app, wake, wakes, application, sender)?
        else {
            return Ok(());
        };
        pending = new_pending;
        app = if reload {
            let queue = load(application.as_ref(), new_app.queue.show_cancelled())?;
            update(new_app, Event::Loaded(queue))
        } else {
            new_app
        };
    }
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

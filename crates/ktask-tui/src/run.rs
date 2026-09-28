//! The loop that owns the terminal and feeds events into [`update`].

use std::io::Write;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use ktask_core::{JournalWatch, Placement, QueueView, TaskDraft, TaskId};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;

use crate::{App, Event, render, update};

/// Something the loop is woken by: a key (or resize) at the terminal, the journal having
/// changed under it, or the process being told to stop.
enum Wake {
    /// An input arrived at the terminal.
    Input(Input),
    /// The journal changed; the queue is stale and worth loading again.
    Changed,
    /// SIGTERM or SIGHUP arrived — the terminal went away, or the operator or the system
    /// asked the process to stop. There is nothing to weigh against a form's content here:
    /// the terminal is leaving regardless, so the loop ends at once.
    Stop,
}

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
/// `watch` blocks until the journal changes; it is polled from a dedicated thread, so a task
/// added, inserted or removed by another process shows in the next frame without the loop
/// itself ever waking on a timer. The terminal is put back as it was on every way out: the
/// operator quitting, SIGTERM or SIGHUP (its terminal going away sends this), or an error.
///
/// # Errors
///
/// Fails when the queue cannot be loaded, a task cannot be removed or the terminal cannot be
/// used.
pub fn run(
    load: impl FnMut(bool) -> Result<QueueView, String>,
    remove: impl FnMut(TaskId) -> Result<(), String>,
    add: impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
    watch: impl JournalWatch + Send + 'static,
) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let result =
        spawn_wakes(watch).and_then(|wakes| drive(&mut terminal, load, remove, add, &wakes));
    ratatui::restore();
    result
}

/// Starts the threads that turn keyboard input, journal changes and a termination signal into
/// a single stream the loop can block on, with no timer of its own.
///
/// # Errors
///
/// Fails when SIGTERM and SIGHUP cannot be watched for.
fn spawn_wakes(watch: impl JournalWatch + Send + 'static) -> Result<Receiver<Wake>, String> {
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
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = sender.send(Wake::Stop);
        }
    });
    Ok(receiver)
}

fn drive(
    terminal: &mut DefaultTerminal,
    mut load: impl FnMut(bool) -> Result<QueueView, String>,
    mut remove: impl FnMut(TaskId) -> Result<(), String>,
    mut add: impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let mut app = update(App::default(), Event::Loaded(load(false)?));
    loop {
        draw(terminal, &app)?;
        if app.quit {
            return Ok(());
        }
        match wakes
            .recv()
            .map_err(|_| "the keyboard and journal-watch threads both stopped".to_owned())?
        {
            Wake::Stop => return Ok(()),
            Wake::Changed => {}
            Wake::Input(input) => {
                let asked = app.show_cancelled;
                if let Some(event) = translate(&input) {
                    app = update(app, event);
                }
                if let Some(id) = app.removal.take() {
                    remove(id)?;
                } else if let Some((draft, placement)) = app.submission.take() {
                    let added = match add(&draft, placement) {
                        Ok(id) => Event::Added(id),
                        Err(problems) => Event::Rejected(problems),
                    };
                    app = update(app, added);
                } else if app.show_cancelled == asked {
                    continue;
                }
            }
        }
        let queue = load(app.show_cancelled)?;
        app = update(app, Event::Loaded(queue));
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

//! The loop that owns the terminal and feeds events into [`update`].

use std::time::Duration;

use ktask_core::{Placement, QueueView, TaskDraft, TaskId};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};

use crate::{App, Event, render, update};

/// How long the loop waits for a key before it loads the queue again, so that a task added
/// from elsewhere shows without a key being pressed.
const REFRESH: Duration = Duration::from_millis(200);

/// Runs the terminal interface until the operator quits.
///
/// `load` fetches the queue to show, with the cancelled tasks when it is told to. It is called
/// at the start, again and again while the interface waits, and when the operator asks for
/// cancelled tasks or stops asking. `remove` removes a task the operator confirmed removing,
/// after which the queue is loaded again. `add` adds the task the operator wrote in the form
/// where the form says, and gives its number, or the reasons it was not added when it was
/// not. The terminal is put back as it was on every way out.
///
/// # Errors
///
/// Fails when the queue cannot be loaded, a task cannot be removed or the terminal cannot be
/// used.
pub fn run(
    load: impl FnMut(bool) -> Result<QueueView, String>,
    remove: impl FnMut(TaskId) -> Result<(), String>,
    add: impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let result = drive(&mut terminal, load, remove, add);
    ratatui::restore();
    result
}

fn drive(
    terminal: &mut DefaultTerminal,
    mut load: impl FnMut(bool) -> Result<QueueView, String>,
    mut remove: impl FnMut(TaskId) -> Result<(), String>,
    mut add: impl FnMut(&TaskDraft, Placement) -> Result<TaskId, Vec<String>>,
) -> Result<(), String> {
    let mut app = update(App::default(), Event::Loaded(load(false)?));
    loop {
        terminal
            .draw(|frame| {
                if let Some(cursor) = render(&app, frame.area(), frame.buffer_mut()) {
                    frame.set_cursor_position(cursor);
                }
            })
            .map_err(|e| format!("cannot draw the screen: {e}"))?;
        if app.quit {
            return Ok(());
        }
        let asked = app.show_cancelled;
        if event::poll(REFRESH).map_err(|e| format!("cannot read the keyboard: {e}"))? {
            let input = event::read().map_err(|e| format!("cannot read the keyboard: {e}"))?;
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

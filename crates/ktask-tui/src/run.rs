//! The loop that owns the terminal and feeds events into [`update`].

use std::time::Duration;

use ktask_core::QueueView;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyEventKind};

use crate::{App, Event, render, update};

/// How long the loop waits for a key before it loads the queue again, so that a task added
/// from elsewhere shows without a key being pressed.
const REFRESH: Duration = Duration::from_millis(200);

/// Runs the terminal interface until the operator quits.
///
/// `load` fetches the queue to show, with the cancelled tasks when it is told to. It is called
/// at the start, again and again while the interface waits, and when the operator asks for
/// cancelled tasks or stops asking. The terminal is put back as it was on every way out.
///
/// # Errors
///
/// Fails when the queue cannot be loaded or the terminal cannot be used.
pub fn run(load: impl FnMut(bool) -> Result<QueueView, String>) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let result = drive(&mut terminal, load);
    ratatui::restore();
    result
}

fn drive(
    terminal: &mut DefaultTerminal,
    mut load: impl FnMut(bool) -> Result<QueueView, String>,
) -> Result<(), String> {
    let mut app = update(App::default(), Event::Loaded(load(false)?));
    loop {
        terminal
            .draw(|frame| render(&app, frame.area(), frame.buffer_mut()))
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
            if app.show_cancelled == asked {
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
        Input::Key(key) if key.kind == KeyEventKind::Press => Some(Event::Key(key.code)),
        Input::Resize(..) => Some(Event::Resize),
        _ => None,
    }
}

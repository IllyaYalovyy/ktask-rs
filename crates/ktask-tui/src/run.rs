//! The loop that owns the terminal and feeds events into [`update`].

use ktask_core::QueueView;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyEventKind};

use crate::{App, Event, render, update};

/// Runs the terminal interface until the operator quits.
///
/// `load` fetches the queue to show. The terminal is put back as it was on every way out.
///
/// # Errors
///
/// Fails when the queue cannot be loaded or the terminal cannot be used.
pub fn run(load: impl FnOnce() -> Result<QueueView, String>) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let result = drive(&mut terminal, load);
    ratatui::restore();
    result
}

fn drive(
    terminal: &mut DefaultTerminal,
    load: impl FnOnce() -> Result<QueueView, String>,
) -> Result<(), String> {
    let mut app = update(App::default(), Event::Loaded(load()?));
    loop {
        terminal
            .draw(|frame| render(&app, frame.area(), frame.buffer_mut()))
            .map_err(|e| format!("cannot draw the screen: {e}"))?;
        if app.quit {
            return Ok(());
        }
        let input = event::read().map_err(|e| format!("cannot read the keyboard: {e}"))?;
        let Some(event) = translate(&input) else {
            continue;
        };
        app = update(app, event);
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

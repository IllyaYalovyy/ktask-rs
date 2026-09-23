//! The thin shell that owns the real terminal.
//!
//! [`run`] puts the terminal into raw mode on the alternate screen, drives
//! [`update`] and [`render`] from key presses, journal events and a clock
//! tick, and puts the terminal back the way it found it however it stops:
//! a normal quit, an error, or a panic. A panic hook restores the terminal
//! *before* the previous hook prints its message, so the message lands on the
//! operator's own screen rather than being swallowed by the alternate one, and
//! the shell is left with echo and line editing back.
//!
//! Everything that decides what happens lives in [`crate::app`]; the loop here
//! is generic over how it draws and where its input comes from, so it runs
//! unchanged under a test backend with scripted input.

use crate::app::{App, render, update};
use crate::event::AppEvent;
use crossterm::cursor::Show;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ktask_core::{Error, Event, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, IsTerminal, Write};
use std::panic;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// How long the loop waits for a key before it ticks and looks at the journal
/// again.
const TICK: Duration = Duration::from_millis(100);

/// Runs the interface on the process's terminal until the operator quits.
///
/// Returns once the terminal has been restored. `rx` carries journal events
/// from an in-process run; the loop drains it on every turn and never treats
/// a disconnected sender as the end of the session.
///
/// # Errors
///
/// Fails without touching the terminal when stdout is not a terminal (see
/// [`is_not_a_terminal`]). Otherwise fails on a terminal I/O error; the
/// terminal is restored first, and a restore failure is reported only when
/// nothing else went wrong.
pub fn run(app: App, rx: Receiver<Event>) -> Result<()> {
    require_terminal(io::stdout().is_terminal())?;
    install_panic_hook();
    let mut out = io::stdout();
    let ran = enter_to(&mut out)
        .and_then(|()| {
            let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            event_loop(
                app,
                move || rx.try_recv().ok(),
                |app| terminal.draw(|frame| render(app, frame)).map(|_| ()),
                |timeout| {
                    if event::poll(timeout)? {
                        event::read().map(Some)
                    } else {
                        Ok(None)
                    }
                },
            )
        })
        .map(|_| ());
    let restored = restore_to(&mut out);
    Ok(ran.and(restored)?)
}

/// Whether `err` is the refusal [`run`] gives when stdout is not a terminal,
/// which the command line reports as a usage error (exit 2).
#[must_use]
pub fn is_not_a_terminal(err: &Error) -> bool {
    matches!(err, Error::Io(e) if e.kind() == io::ErrorKind::Unsupported)
}

fn require_terminal(stdout_is_terminal: bool) -> Result<()> {
    if stdout_is_terminal {
        Ok(())
    } else {
        Err(Error::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "ktask-rs tui needs a terminal, but stdout is not one; \
             use `ktask-rs status` for scripting",
        )))
    }
}

/// Enters raw mode and the alternate screen.
///
/// If the second step fails the first is undone, so a failed entry leaves the
/// terminal as it was.
fn enter_to(out: &mut impl Write) -> io::Result<()> {
    enable_raw_mode()?;
    if let Err(err) = execute!(out, EnterAlternateScreen) {
        let _ = restore_to(out);
        return Err(err);
    }
    Ok(())
}

/// Leaves the alternate screen, shows the cursor and leaves raw mode.
///
/// Every step is attempted even if an earlier one fails, and doing it twice is
/// harmless: it runs from the panic hook and again on the normal path.
fn restore_to(out: &mut impl Write) -> io::Result<()> {
    let screen = execute!(out, LeaveAlternateScreen, Show);
    let raw = disable_raw_mode();
    screen.and(raw)
}

/// Makes a panic restore the terminal before anything is printed.
///
/// The previous hook still runs afterwards, so the panic message and any
/// backtrace appear as they always did, on the restored screen.
pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore_to(&mut io::stdout());
        previous(info);
    }));
}

/// Draws, waits for one input, folds it and whatever the journal produced into
/// the state, and repeats until the operator quits. Returns the final state.
fn event_loop(
    mut app: App,
    mut journal: impl FnMut() -> Option<Event>,
    mut draw: impl FnMut(&App) -> io::Result<()>,
    mut poll: impl FnMut(Duration) -> io::Result<Option<TermEvent>>,
) -> io::Result<App> {
    loop {
        draw(&app)?;
        match poll(TICK)? {
            Some(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => {
                if quits(&app, &key) {
                    return Ok(app);
                }
                app = update(app, AppEvent::Key(key));
            }
            Some(TermEvent::Resize(columns, rows)) => {
                app = update(app, AppEvent::Resize(columns, rows));
            }
            _ => {}
        }
        while let Some(event) = journal() {
            app = update(app, AppEvent::Core(event));
        }
        app = update(app, AppEvent::Tick);
    }
}

/// Whether `key` ends the session: Ctrl-C always, and `q` unless an overlay is
/// open, where it closes the overlay instead, or a search is being typed,
/// where it is a letter of the search.
fn quits(app: &App, key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('q') => {
            key.modifiers.is_empty() && app.overlay.is_none() && !app.logs.is_typing()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Overlay;
    use ktask_core::{EventKind, EventSeq, TaskId};
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use time::OffsetDateTime;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> TermEvent {
        TermEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn press(c: char) -> TermEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Runs the loop over `script`, one input per turn, on a test backend.
    /// Returns the final state and the size the app had at each draw.
    fn drive(
        app: App,
        rx: &Receiver<Event>,
        script: Vec<TermEvent>,
    ) -> (io::Result<App>, Vec<(u16, u16)>) {
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).expect("test backend");
        let mut script: VecDeque<TermEvent> = script.into();
        let mut sizes = Vec::new();
        let outcome = event_loop(
            app,
            || rx.try_recv().ok(),
            |app| {
                sizes.push(app.size);
                terminal
                    .draw(|frame| render(app, frame))
                    .map(|_| ())
                    .map_err(|never| match never {})
            },
            |_| Ok(script.pop_front()),
        );
        (outcome, sizes)
    }

    fn queued(seq: u64, id: u32, title: &str) -> Event {
        Event {
            seq: EventSeq::new(seq),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(id)),
            kind: EventKind::TaskQueued {
                title: title.into(),
            },
        }
    }

    #[test]
    fn q_quits_and_the_state_at_that_moment_is_returned() {
        let (_tx, rx) = mpsc::channel();
        let (outcome, sizes) = drive(
            App::new((20, 5)),
            &rx,
            vec![
                TermEvent::Resize(30, 9),
                press('q'),
                TermEvent::Resize(1, 1),
            ],
        );
        assert_eq!(outcome.expect("loop").size, (30, 9));
        assert_eq!(sizes, [(20, 5), (30, 9)]);
    }

    #[test]
    fn ctrl_c_quits_even_with_an_overlay_open() {
        let mut app = App::new((20, 5));
        app.overlay = Some(Overlay::KeyMap);
        let (_tx, rx) = mpsc::channel();
        let (outcome, sizes) = drive(
            app,
            &rx,
            vec![key(KeyCode::Char('c'), KeyModifiers::CONTROL)],
        );
        assert!(outcome.is_ok());
        assert_eq!(sizes.len(), 1);
    }

    #[test]
    fn q_does_not_quit_while_an_overlay_is_open() {
        let mut app = App::new((20, 5));
        app.overlay = Some(Overlay::KeyMap);
        assert!(!quits(
            &app,
            &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
        ));
        app.overlay = None;
        assert!(quits(
            &app,
            &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn q_is_a_letter_of_the_search_while_one_is_typed_but_ctrl_c_still_quits() {
        let mut app = App::new((20, 5));
        app.screen = crate::types::Screen::Logs;
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app = update(
            app,
            AppEvent::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );
        assert!(app.logs.is_typing());
        assert!(!quits(&app, &q));
        assert!(quits(&app, &ctrl_c));
        app = update(
            app,
            AppEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(quits(&app, &q));
    }

    #[test]
    fn keys_other_than_the_quit_keys_do_not_quit() {
        let app = App::new((20, 5));
        for code in [KeyCode::Char('c'), KeyCode::Char('x'), KeyCode::Esc] {
            assert!(!quits(&app, &KeyEvent::new(code, KeyModifiers::NONE)));
        }
        assert!(!quits(
            &app,
            &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn a_key_release_is_ignored_so_quitting_needs_a_press() {
        let (_tx, rx) = mpsc::channel();
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        let (outcome, sizes) = drive(
            App::new((20, 5)),
            &rx,
            vec![TermEvent::Key(release), press('q')],
        );
        assert!(outcome.is_ok());
        assert_eq!(sizes.len(), 2);
    }

    #[test]
    fn journal_events_reach_the_state_on_the_next_turn() {
        let (tx, rx) = mpsc::channel();
        tx.send(queued(1, 1, "First")).expect("send");
        tx.send(queued(2, 2, "Second")).expect("send");
        let (outcome, _) = drive(
            App::new((20, 5)),
            &rx,
            vec![TermEvent::FocusGained, press('q')],
        );
        let titles: Vec<String> = outcome
            .expect("loop")
            .tasks
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, ["First", "Second"]);
    }

    #[test]
    fn a_disconnected_journal_channel_does_not_end_the_session() {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        let (outcome, sizes) = drive(
            App::new((20, 5)),
            &rx,
            vec![TermEvent::FocusGained, TermEvent::FocusLost, press('q')],
        );
        assert!(outcome.is_ok());
        assert_eq!(sizes.len(), 3);
    }

    #[test]
    fn an_input_error_ends_the_loop_with_that_error() {
        let err = event_loop(
            App::new((20, 5)),
            || None,
            |_| Ok(()),
            |_| Err(io::Error::other("input broke")),
        )
        .expect_err("input error propagates");
        assert_eq!(err.to_string(), "input broke");
    }

    #[test]
    fn a_draw_error_ends_the_loop_before_any_input_is_read() {
        let mut polled = false;
        let err = event_loop(
            App::new((20, 5)),
            || None,
            |_| Err(io::Error::other("draw broke")),
            |_| {
                polled = true;
                Ok(None)
            },
        )
        .expect_err("draw error propagates");
        assert_eq!(err.to_string(), "draw broke");
        assert!(!polled);
    }

    #[test]
    fn the_loop_waits_one_tick_for_input() {
        let mut waited = None;
        let outcome = event_loop(
            App::new((20, 5)),
            || None,
            |_| Ok(()),
            |timeout| {
                waited = Some(timeout);
                Ok(Some(press('q')))
            },
        );
        assert!(outcome.is_ok());
        assert_eq!(waited, Some(TICK));
    }

    #[test]
    fn restoring_leaves_the_alternate_screen_and_shows_the_cursor() {
        let mut out = Vec::new();
        // Raw mode cannot be switched off without a terminal, so the result is
        // not asserted; what matters is that the screen steps still ran.
        let _ = restore_to(&mut out);
        let written = String::from_utf8(out).expect("escape sequences are ASCII");
        assert_eq!(written, "\x1b[?1049l\x1b[?25h");
    }

    #[test]
    fn a_non_terminal_stdout_is_refused_with_a_message_that_says_why() {
        let err = require_terminal(false).expect_err("not a terminal");
        assert!(is_not_a_terminal(&err));
        let message = err.to_string();
        assert!(message.contains("needs a terminal"), "{message}");
        assert!(message.contains("stdout"), "{message}");
        assert!(require_terminal(true).is_ok());
    }

    #[test]
    fn other_errors_are_not_mistaken_for_a_missing_terminal() {
        assert!(!is_not_a_terminal(&Error::Io(io::Error::other("disk"))));
        assert!(!is_not_a_terminal(&Error::NotFound {
            what: "x".to_owned()
        }));
    }
}

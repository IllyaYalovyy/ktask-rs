//! A headless harness that drives the interface without a terminal.
//!
//! [`Harness`] owns an [`App`] and a ratatui [`TestBackend`]. Every event sent
//! to it goes through the real [`update`] and the real [`render`], so what a
//! test reads back with [`Harness::text`] is what an operator would see, with
//! no TTY, no raw mode and no timing involved.

use crate::app::{App, render, update};
use crate::event::AppEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

/// An [`App`] rendered into an in-memory terminal after every event.
#[derive(Debug)]
pub struct Harness {
    app: App,
    terminal: Terminal<TestBackend>,
}

impl Harness {
    /// A fresh interface on a `w` by `h` terminal, already drawn once.
    ///
    /// # Panics
    ///
    /// Never in practice: drawing to a [`TestBackend`] cannot fail.
    #[must_use]
    pub fn new(w: u16, h: u16) -> Harness {
        let mut harness = Harness {
            app: App::new((w, h)),
            terminal: Terminal::new(TestBackend::new(w, h)).expect("test backend is infallible"),
        };
        harness.draw();
        harness
    }

    /// Feeds `ev` to the interface and redraws.
    ///
    /// A [`AppEvent::Resize`] resizes the terminal too, as a real one would
    /// before the next frame.
    pub fn send(&mut self, ev: AppEvent) {
        self.app = update(self.app.clone(), ev);
        let (w, h) = self.app.size;
        self.terminal.backend_mut().resize(w, h);
        self.draw();
    }

    /// Presses the plain key `c`, with no modifiers.
    pub fn key(&mut self, c: char) {
        self.send(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }

    /// The interface state as it stands after the last event.
    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    /// The screen as last drawn.
    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        self.terminal.backend().buffer()
    }

    /// The screen as last drawn, one line per row, joined by newlines.
    ///
    /// Rows keep their full width, so trailing blanks are part of the text.
    #[must_use]
    pub fn text(&self) -> String {
        let buffer = self.buffer();
        let area = buffer.area;
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw(&mut self) {
        self.terminal
            .draw(|frame| render(&self.app, frame))
            .expect("test backend is infallible");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Overlay;

    #[test]
    fn testing_new_draws_the_queue_header_padded_to_the_terminal_size() {
        let harness = Harness::new(10, 3);
        assert_eq!(harness.text(), "1 Queue   \nNo tasks q\n          ");
    }

    #[test]
    fn testing_key_reaches_update_as_a_plain_key_event_and_the_screen_is_redrawn() {
        let mut harness = Harness::new(20, 3);
        harness.key('x');
        // Key bindings are not wired yet, so the key changes nothing visible;
        // the frame is still the queue's.
        assert!(harness.text().starts_with("1 Queue"));
        assert_eq!(harness.app(), &App::new((20, 3)));
    }

    #[test]
    fn testing_send_renders_state_changes_made_by_the_event() {
        let mut harness = Harness::new(20, 3);
        harness.app.overlay = Some(Overlay::KeyMap);
        harness.send(AppEvent::Tick);
        assert!(harness.text().contains("Key map"));
    }

    #[test]
    fn testing_resize_resizes_the_buffer_and_the_text() {
        let mut harness = Harness::new(10, 3);
        harness.send(AppEvent::Resize(6, 2));
        assert_eq!(harness.buffer().area.width, 6);
        assert_eq!(harness.buffer().area.height, 2);
        assert_eq!(
            harness.text(),
            format!("{}\nNo tas", "1 Queue".get(..6).unwrap())
        );
        assert_eq!(harness.app().size, (6, 2));
    }

    #[test]
    fn testing_text_has_one_line_per_row() {
        let harness = Harness::new(4, 5);
        assert_eq!(harness.text().split('\n').count(), 5);
        assert!(harness.text().split('\n').all(|l| l.chars().count() == 4));
    }

    #[test]
    fn testing_degenerate_sizes_do_not_panic() {
        let mut harness = Harness::new(0, 0);
        assert_eq!(harness.text(), "");
        harness.key('q');
        harness.send(AppEvent::Resize(1, 1));
        assert_eq!(harness.text(), "1");
        harness.send(AppEvent::Resize(0, 5));
        harness.send(AppEvent::Resize(80, 24));
        assert!(harness.text().starts_with("1 Queue"));
    }
}

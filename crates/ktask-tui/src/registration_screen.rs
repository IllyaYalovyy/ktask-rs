//! The registration screen: asks for a name to register the current directory under, after
//! its folder name turned out to already be taken by another registered project. This is the
//! whole screen — there is no queue behind it to fall back to, so it is only ever open before
//! the first queue is loaded.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::text::TextArea;

/// What the registration screen's frame says at the bottom: there is no queue behind it to
/// cancel back onto, so Esc quits rather than cancelling.
const REGISTRATION_KEYS: &str = " Ctrl-S register · Esc quit ";

/// What a key on the registration screen asks the rest of the application to do, when it is
/// not something the screen answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Leave every screen: there is no queue to fall back to.
    Quit,
    /// Register the current directory under this name.
    Submit(String),
}

/// The registration screen's own state: the name being typed to register the directory
/// under, and why a name is needed at all — the refusal that opened this screen, or, after a
/// name it was submitted with was refused too, the same words `ktask-rs project register
/// --name` gives for the same conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistrationScreen {
    problem: String,
    name: TextArea,
}

impl RegistrationScreen {
    /// A screen open on `problem`, with an empty name.
    pub(crate) fn new(problem: String) -> Self {
        Self {
            problem,
            name: TextArea::new(false),
        }
    }

    /// The name typed so far.
    #[cfg(test)]
    pub(crate) fn name_text(&self) -> String {
        self.name.text()
    }

    /// The screen's own problem line right now.
    #[cfg(test)]
    pub(crate) fn problem(&self) -> &str {
        &self.problem
    }

    /// The screen once its submission registered nothing, for this reason — the same words
    /// `ktask-rs project register --name` gives for the same conflict: it keeps what was
    /// typed and shows this instead.
    pub(crate) fn rejected(self, problem: String) -> Self {
        Self { problem, ..self }
    }

    /// A key on the registration screen: Esc quits, since there is no queue to fall back to;
    /// anything else is typed into the name.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Esc => (self, Some(Request::Quit)),
            _ => (
                Self {
                    name: self.name.press(key),
                    ..self
                },
                None,
            ),
        }
    }

    /// A Ctrl-letter on the registration screen: `s` submits the typed name, for the loop to
    /// register the current directory under.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        match letter {
            's' => {
                let name = self.name.text();
                (self, Some(Request::Submit(name)))
            }
            _ => (self, None),
        }
    }

    /// What the frame's bottom border says while the registration screen is showing.
    pub(crate) fn footer_keys() -> &'static str {
        REGISTRATION_KEYS
    }

    /// Draws the registration screen over the whole of `area`: the refusal that made a name
    /// necessary, wrapped to fit — a path can run well past the width of the screen — over
    /// the top of it, and the name field pinned to its last two rows regardless of how many
    /// rows that takes. Returns where the cursor goes, in the name.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Position {
        let [info, prompt] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
        let info_lines = vec![
            Line::styled(
                "This directory cannot be opened",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::default(),
            Line::from(self.problem.clone()),
        ];
        Paragraph::new(info_lines)
            .wrap(Wrap { trim: false })
            .render(info, buf);
        let prompt_lines = vec![
            Line::from("Register it under this name instead:"),
            Line::from(format!("> {}", self.name.text())),
        ];
        Paragraph::new(prompt_lines).render(prompt, buf);
        Position::new(
            (prompt.x + 2 + u16::try_from(self.name.cursor().1).unwrap_or(u16::MAX))
                .min(prompt.x + prompt.width.saturating_sub(1)),
            prompt.y + 1,
        )
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn a_new_screen_holds_the_problem_it_opened_on_with_an_empty_name() {
        let screen = RegistrationScreen::new("conflict".to_owned());
        assert_eq!(screen.problem(), "conflict");
        assert_eq!(screen.name_text(), "");
    }

    #[test]
    fn typing_edits_the_name() {
        let screen = "my-app".chars().fold(
            RegistrationScreen::new("conflict".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        assert_eq!(screen.name_text(), "my-app");
    }

    #[test]
    fn esc_asks_to_quit() {
        let (_, request) = RegistrationScreen::new("conflict".to_owned()).key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Quit));
    }

    #[test]
    fn ctrl_s_submits_the_typed_name() {
        let screen = "my-app".chars().fold(
            RegistrationScreen::new("conflict".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        let (_, request) = screen.ctrl('s');
        assert_eq!(request, Some(Request::Submit("my-app".to_owned())));
    }

    #[test]
    fn rejected_keeps_the_name_and_shows_why() {
        let screen = "taken".chars().fold(
            RegistrationScreen::new("conflict".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        let screen = screen.rejected("that name is taken too".to_owned());
        assert_eq!(screen.problem(), "that name is taken too");
        assert_eq!(screen.name_text(), "taken");
    }

    fn drawn(screen: &RegistrationScreen, width: u16, height: u16) -> (Vec<String>, Position) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let cursor = screen.draw(area, &mut buf);
        let rows = (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        (rows, cursor)
    }

    fn row(rows: &[String], y: usize) -> &str {
        rows[y].trim_end()
    }

    #[test]
    fn shows_the_conflict_and_the_name_field_with_the_cursor_in_it() {
        let screen = RegistrationScreen::new(
            "that name is already registered for /elsewhere/app".to_owned(),
        );
        let (rows, cursor) = drawn(&screen, 60, 6);
        assert_eq!(row(&rows, 0), "This directory cannot be opened");
        assert_eq!(
            row(&rows, 2),
            "that name is already registered for /elsewhere/app"
        );
        assert_eq!(row(&rows, 4), "Register it under this name instead:");
        assert_eq!(row(&rows, 5), ">");
        assert_eq!(cursor, Position::new(2, 5));
    }

    #[test]
    fn typing_a_name_shows_it_with_the_cursor_after_it() {
        let screen = "my-app-2".chars().fold(
            RegistrationScreen::new("conflict".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        let (rows, cursor) = drawn(&screen, 60, 6);
        assert_eq!(row(&rows, 5), "> my-app-2");
        assert_eq!(cursor, Position::new(10, 5));
    }
}

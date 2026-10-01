//! The settings screen: every project setting, the field each is edited in, and how it is
//! drawn. Covers the queue while it is open.

use ktask_core::{
    STEP_COMMIT, STEP_HEALTH_CHECK, STEP_PUSH, STEP_REVIEW, STEP_SYNC, STEP_TESTING, SettingView,
};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::scroll::first_shown;
use crate::text::TextArea;

/// What the settings screen's frame says at the bottom.
const SETTINGS_KEYS: &str = " Tab, Shift-Tab field · Ctrl-S save · Esc cancel ";

/// Every setting whose field is an on/off switch rather than free text.
const TOGGLE_NAMES: [&str; 6] = [
    STEP_SYNC,
    STEP_HEALTH_CHECK,
    STEP_REVIEW,
    STEP_TESTING,
    STEP_COMMIT,
    STEP_PUSH,
];

/// How many lines one field's own block takes: a blank line, its label, and its value.
const FIELD_HEIGHT: usize = 3;

/// Whether the setting called `name` is an on/off switch: edited with Space, Left or Right,
/// and unable to hold anything but `"on"` or `"off"` — never free text.
fn is_toggle(name: &str) -> bool {
    TOGGLE_NAMES.contains(&name)
}

/// `"on"` when `value` is `"off"`, `"off"` otherwise — an on/off field's value after it is
/// switched.
fn toggled(value: &str) -> &'static str {
    if value == "on" { "off" } else { "on" }
}

/// The label a setting's own field is shown under, by name.
fn label(name: &str) -> &'static str {
    match name {
        "health-check" => "Health check command",
        "tracked-branch" => "Tracked branch (remote/branch)",
        "step-sync" => "Sync step (on/off)",
        "step-health-check" => "Health check step (on/off)",
        "step-review" => "Review step (on/off)",
        "step-testing" => "Testing step (on/off)",
        "step-commit" => "Commit step (on/off)",
        "step-push" => "Push step (on/off)",
        "max-attempts" => "Max attempts",
        "resolver-provider" => "Resolver provider",
        "resolver-model" => "Resolver model",
        _ => "Attempt timeout, in seconds",
    }
}

/// One setting's own field in the settings screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingField {
    /// The setting's name, submitted alongside its value.
    name: &'static str,
    /// Whether the value on show, before anything is typed, was the built-in default rather
    /// than one the project had set.
    is_default: bool,
    /// The value, as typed so far.
    text: TextArea,
    /// Whether this field is an on/off switch: only Space, Left or Right change it, flipping
    /// it between `"on"` and `"off"` — it can never hold anything typed.
    is_toggle: bool,
}

/// What a key on the settings screen asks the rest of the application to do, when it is not
/// something the screen answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the screen without saving anything.
    Close,
    /// Save this setting to this value.
    Submit(&'static str, String),
}

/// The settings screen's own state: one field per project setting, with the focus on one of
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsScreen {
    fields: Vec<SettingField>,
    focus: usize,
    problem: Option<String>,
}

impl SettingsScreen {
    /// A settings screen opened on `views`' current values, the focus on the first.
    pub(crate) fn new(views: &[SettingView]) -> Self {
        Self {
            fields: views
                .iter()
                .map(|view| SettingField {
                    name: view.name,
                    is_default: view.is_default,
                    text: TextArea::with_text(false, &view.value),
                    is_toggle: is_toggle(view.name),
                })
                .collect(),
            focus: 0,
            problem: None,
        }
    }

    /// The focused field's setting name, when there is a field.
    #[cfg(test)]
    fn name(&self) -> Option<&'static str> {
        self.fields.get(self.focus).map(|field| field.name)
    }

    /// Why the last submission changed nothing, when it was refused.
    #[cfg(test)]
    pub(crate) fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The focused field's value, when there is a field.
    #[cfg(test)]
    fn value(&self) -> String {
        self.fields
            .get(self.focus)
            .map(|field| field.text.text())
            .unwrap_or_default()
    }

    /// The screen once its last submission was refused, showing why.
    pub(crate) fn rejected(self, message: String) -> Self {
        Self {
            problem: Some(message),
            ..self
        }
    }

    /// A key on the settings screen: Esc closes it, Tab and Shift-Tab move the focus, and
    /// every other key is pressed in the focused field.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Esc => (self, Some(Request::Close)),
            KeyCode::Tab => (self.moved(true), None),
            KeyCode::BackTab => (self.moved(false), None),
            _ => (self.pressed(key), None),
        }
    }

    /// A Ctrl-letter on the settings screen: `s` submits the focused field's name and value,
    /// for the loop to save; the screen stays open until the loop's answer closes it or shows
    /// why it did not.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        match letter {
            's' => {
                let request = self
                    .fields
                    .get(self.focus)
                    .map(|field| Request::Submit(field.name, field.text.text()));
                (self, request)
            }
            _ => (self, None),
        }
    }

    /// The focused field after `key` was pressed in it: an on/off field only answers to
    /// Space, Left and Right, which flip it; every other field is free text, edited as typed.
    fn pressed(mut self, key: KeyCode) -> Self {
        if let Some(field) = self.fields.get_mut(self.focus) {
            if field.is_toggle {
                if matches!(key, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) {
                    field.text = TextArea::with_text(false, toggled(&field.text.text()));
                }
            } else {
                field.text = field.text.clone().press(key);
            }
        }
        self
    }

    /// The screen with the focus on the next field, or the previous one; it wraps around.
    fn moved(mut self, forward: bool) -> Self {
        let count = self.fields.len();
        if count == 0 {
            return self;
        }
        self.focus = if forward {
            (self.focus + 1) % count
        } else {
            (self.focus + count - 1) % count
        };
        self
    }

    /// What the frame's bottom border says while the settings screen is showing.
    pub(crate) fn footer_keys() -> &'static str {
        SETTINGS_KEYS
    }

    /// Draws the settings screen over the whole of `area`, and returns where the cursor goes:
    /// in the field that has the focus, scrolled into view when the terminal is too short to
    /// show every field at once.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Position {
        let header = self.header_lines();
        let header_height = u16::try_from(header.len())
            .unwrap_or(u16::MAX)
            .min(area.height);
        let [header_area, fields_area] =
            Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
        Paragraph::new(header).render(header_area, buf);

        let (lines, cursor) = self.field_lines_and_cursor(fields_area);
        Paragraph::new(lines).render(fields_area, buf);
        cursor
    }

    /// "Settings", plus the last submission's refusal above the fields, when there is one.
    fn header_lines(&self) -> Vec<Line<'static>> {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut header = vec![Line::styled("Settings", bold)];
        if let Some(problem) = &self.problem {
            header.push(Line::styled(format!("! {problem}"), bold));
        }
        header
    }

    /// Every field's own lines that fit `fields_area`, scrolled so the focused one is always
    /// among them, and where its cursor goes.
    fn field_lines_and_cursor(&self, fields_area: Rect) -> (Vec<Line<'static>>, Position) {
        let mut cursor = Position::new(fields_area.x, fields_area.y);
        if self.fields.is_empty() {
            return (Vec::new(), cursor);
        }
        let heights = vec![FIELD_HEIGHT; self.fields.len()];
        let first = first_shown(&heights, Some(self.focus), usize::from(fields_area.height));

        let mut lines = Vec::new();
        for (index, field) in self.fields.iter().enumerate().skip(first) {
            if usize::from(fields_area.height).saturating_sub(lines.len()) < FIELD_HEIGHT {
                break;
            }
            let focused = index == self.focus;
            push_field(&mut lines, field, focused);
            if focused {
                cursor = field_cursor(fields_area, lines.len() - 1, field);
            }
        }
        (lines, cursor)
    }
}

/// The field's marker, ahead of the typed value, when it has the focus.
const FOCUSED_PREFIX: &str = "> ";
/// The field's marker, ahead of the typed value, when it does not have the focus.
const UNFOCUSED_PREFIX: &str = "  ";

/// Pushes one field's own three lines onto `lines`: a blank line, its label with whether it is
/// still the default, and its value marked with the focus or not.
fn push_field(lines: &mut Vec<Line<'static>>, field: &SettingField, focused: bool) {
    let kind = if field.is_default {
        "default"
    } else {
        "custom"
    };
    lines.push(Line::default());
    lines.push(Line::from(format!("{} ({kind}):", label(field.name))));
    let prefix = if focused {
        FOCUSED_PREFIX
    } else {
        UNFOCUSED_PREFIX
    };
    lines.push(Line::from(format!("{prefix}{}", field.text.text())));
}

/// Where the cursor goes for the focused `field`, whose value sits at `row` inside
/// `fields_area`.
fn field_cursor(fields_area: Rect, row: usize, field: &SettingField) -> Position {
    let col = FOCUSED_PREFIX.chars().count() + field.text.cursor().1;
    Position::new(
        fields_area.x
            + u16::try_from(col)
                .unwrap_or(u16::MAX)
                .min(fields_area.width.saturating_sub(1)),
        fields_area.y
            + u16::try_from(row)
                .unwrap_or(u16::MAX)
                .min(fields_area.height.saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;

    fn views() -> Vec<SettingView> {
        vec![
            SettingView {
                name: "attempt-timeout",
                value: "14400".to_owned(),
                is_default: true,
            },
            SettingView {
                name: "health-check",
                value: String::new(),
                is_default: true,
            },
        ]
    }

    fn views_with_a_toggle() -> Vec<SettingView> {
        vec![SettingView {
            name: STEP_REVIEW,
            value: "on".to_owned(),
            is_default: true,
        }]
    }

    #[test]
    fn a_new_screen_opens_on_the_first_field_with_its_value_and_no_problem() {
        let screen = SettingsScreen::new(&views());
        assert_eq!(screen.name(), Some("attempt-timeout"));
        assert_eq!(screen.value(), "14400");
        assert!(screen.fields[0].is_default);
        assert_eq!(screen.problem, None);
    }

    #[test]
    fn typing_changes_the_focused_fields_value_only() {
        let screen = SettingsScreen::new(&views());
        let screen = "600"
            .chars()
            .fold(screen, |screen, c| screen.key(KeyCode::Char(c)).0);
        assert_eq!(screen.value(), "14400600");
        let (screen, _) = screen.key(KeyCode::Backspace);
        assert_eq!(screen.value(), "1440060");
        assert_eq!(screen.fields[1].text.text(), "");
    }

    #[test]
    fn tab_and_shift_tab_move_the_focus_between_settings_and_wrap() {
        let screen = SettingsScreen::new(&views());
        let (screen, _) = screen.key(KeyCode::Tab);
        assert_eq!(screen.name(), Some("health-check"));
        let (screen, _) = screen.key(KeyCode::Tab);
        assert_eq!(screen.name(), Some("attempt-timeout"));
        let (screen, _) = screen.key(KeyCode::BackTab);
        assert_eq!(screen.name(), Some("health-check"));
    }

    #[test]
    fn typing_after_tab_edits_the_newly_focused_field_only() {
        let screen = SettingsScreen::new(&views());
        let (screen, _) = screen.key(KeyCode::Tab);
        let screen = "cargo test"
            .chars()
            .fold(screen, |screen, c| screen.key(KeyCode::Char(c)).0);
        assert_eq!(screen.value(), "cargo test");
        assert_eq!(screen.fields[0].text.text(), "14400");
    }

    #[test]
    fn max_attempts_and_resolver_settings_show_their_own_labels_not_the_fallback() {
        let views = vec![
            SettingView {
                name: "max-attempts",
                value: "3".to_owned(),
                is_default: true,
            },
            SettingView {
                name: "resolver-provider",
                value: "echo".to_owned(),
                is_default: true,
            },
            SettingView {
                name: "resolver-model",
                value: String::new(),
                is_default: true,
            },
        ];
        let screen = SettingsScreen::new(&views);
        let (rows, _) = drawn(&screen, 60, 12);
        assert_eq!(row(&rows, 2), "Max attempts (default):");
        assert_eq!(row(&rows, 5), "Resolver provider (default):");
        assert_eq!(row(&rows, 8), "Resolver model (default):");
    }

    #[test]
    fn an_on_off_field_is_marked_a_toggle_and_the_others_are_not() {
        let screen = SettingsScreen::new(&views_with_a_toggle());
        assert!(screen.fields[0].is_toggle);
        let screen = SettingsScreen::new(&views());
        assert!(!screen.fields[0].is_toggle);
        assert!(!screen.fields[1].is_toggle);
    }

    #[test]
    fn space_left_and_right_flip_an_on_off_field_and_it_holds_nothing_typed() {
        let screen = SettingsScreen::new(&views_with_a_toggle());
        assert_eq!(screen.value(), "on");
        let (screen, _) = screen.key(KeyCode::Char(' '));
        assert_eq!(screen.value(), "off");
        let (screen, _) = screen.key(KeyCode::Left);
        assert_eq!(screen.value(), "on");
        let (screen, _) = screen.key(KeyCode::Right);
        assert_eq!(screen.value(), "off");
        let screen = "off"
            .chars()
            .fold(screen, |screen, c| screen.key(KeyCode::Char(c)).0);
        assert_eq!(screen.value(), "off");
        let (screen, _) = screen.key(KeyCode::Backspace);
        assert_eq!(screen.value(), "off");
    }

    #[test]
    fn esc_asks_to_close_the_screen() {
        let (_, request) = SettingsScreen::new(&views()).key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn ctrl_s_submits_the_focused_fields_name_and_value_and_leaves_the_screen_open() {
        let screen = SettingsScreen::new(&views());
        let (screen, request) = screen.ctrl('s');
        assert_eq!(
            request,
            Some(Request::Submit("attempt-timeout", "14400".to_owned()))
        );
        let (screen, _) = screen.key(KeyCode::Tab);
        let (_, request) = screen.ctrl('s');
        assert_eq!(
            request,
            Some(Request::Submit("health-check", String::new()))
        );
    }

    #[test]
    fn rejected_keeps_the_screen_open_with_its_value_and_shows_why() {
        let screen = SettingsScreen::new(&views());
        let (screen, _) = screen.key(KeyCode::Char('x'));
        let screen = screen.rejected("not a number".to_owned());
        assert_eq!(screen.problem.as_deref(), Some("not a number"));
        assert_eq!(screen.value(), "14400x");
    }

    fn drawn(screen: &SettingsScreen, width: u16, height: u16) -> (Vec<String>, Position) {
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
    fn the_screen_shows_the_value_and_default_tag() {
        let screen = SettingsScreen::new(&views());
        let (rows, cursor) = drawn(&screen, 60, 9);
        assert_eq!(row(&rows, 0), "Settings");
        assert_eq!(row(&rows, 2), "Attempt timeout, in seconds (default):");
        assert_eq!(row(&rows, 3), "> 14400");
        assert_eq!(row(&rows, 5), "Health check command (default):");
        assert_eq!(cursor, Position::new(7, 3));
    }

    #[test]
    fn tab_moves_the_cursor_to_the_next_fields_value() {
        let screen = SettingsScreen::new(&views());
        let (screen, _) = screen.key(KeyCode::Tab);
        let (rows, cursor) = drawn(&screen, 60, 9);
        assert_eq!(row(&rows, 6), ">");
        assert_eq!(cursor, Position::new(2, 6));
    }

    #[test]
    fn a_refused_setting_shows_the_reason_above_the_field() {
        let screen = SettingsScreen::new(&[SettingView {
            name: "attempt-timeout",
            value: "60".to_owned(),
            is_default: false,
        }])
        .rejected("not a number".to_owned());
        let (rows, _) = drawn(&screen, 60, 7);
        assert_eq!(row(&rows, 1), "! not a number");
        assert_eq!(row(&rows, 3), "Attempt timeout, in seconds (custom):");
        assert_eq!(row(&rows, 4), "> 60");
    }
}

//! The task form: a new task's own fields, where the focus is, a question to discard it
//! after a first Ctrl-C finds something typed, and how it is drawn. Covers the queue while it
//! is open.

use ktask_core::{Placement, TaskDraft, TaskKind};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::text::TextArea;
use crate::widgets::key_map;

/// What the form's frame says at the bottom: the keys that are not typing.
const FORM_KEYS: &str =
    " Ctrl-S add · Esc cancel · Tab, Shift-Tab field · Ctrl-N, Ctrl-D criterion ";

/// What the form's frame says at the bottom while it asks to discard the task.
const DISCARD_KEYS: &str = " y discard · n, Esc keep writing ";

/// The keys shown by `?` while the discard question is asking.
const DISCARD_QUESTION_KEYS: [(&str, &str); 2] = [("y", "discard it"), ("n, Esc", "keep writing")];

/// The field the keys go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Title,
    Kind,
    Links,
    Body,
    /// The criterion at this index.
    Criterion(usize),
}

/// A task being written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Form {
    title: TextArea,
    kind: TaskKind,
    /// Links are written on one line, apart by spaces: a link holds no space.
    links: TextArea,
    body: TextArea,
    criteria: Vec<TextArea>,
    focus: Focus,
    /// Where the task goes in the queue.
    placement: Placement,
    /// Why the last submission added nothing.
    problems: Vec<String>,
}

impl Form {
    /// An empty form with one empty criterion, on its title, for a task that goes at
    /// `placement`.
    fn new(placement: Placement) -> Self {
        Self {
            title: TextArea::new(false),
            kind: TaskKind::default(),
            links: TextArea::new(false),
            body: TextArea::new(true),
            criteria: vec![TextArea::new(false)],
            focus: Focus::Title,
            placement,
            problems: Vec::new(),
        }
    }

    /// Whether anything has been typed into the form: an empty title, body, links and
    /// criteria are nothing a discard would lose.
    fn has_content(&self) -> bool {
        !self.title.text().is_empty()
            || !self.body.text().is_empty()
            || !self.links.text().is_empty()
            || self
                .criteria
                .iter()
                .any(|criterion| !criterion.text().is_empty())
    }

    /// The task the form holds, as typed.
    fn draft(&self) -> TaskDraft {
        TaskDraft {
            title: self.title.text(),
            body: self.body.text(),
            criteria: self.criteria.iter().map(TextArea::text).collect(),
            kind: self.kind,
            links: self
                .links
                .text()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        }
    }

    /// The form with the focus on the next field, or the previous one; it wraps around.
    fn moved(self, forward: bool) -> Self {
        let mut order = vec![Focus::Title, Focus::Kind, Focus::Links, Focus::Body];
        order.extend((0..self.criteria.len()).map(Focus::Criterion));
        let at = order
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % order.len()
        } else {
            (at + order.len() - 1) % order.len()
        };
        let focus = order.get(next).copied().unwrap_or(self.focus);
        Self { focus, ..self }
    }

    /// The form with an empty criterion after the last, and the focus on it.
    fn with_criterion(mut self) -> Self {
        self.criteria.push(TextArea::new(false));
        self.focus = Focus::Criterion(self.criteria.len() - 1);
        self
    }

    /// The form without the criterion the focus is on; the focus goes to the one that takes
    /// its place, or the one before it, or the body when none is left. Elsewhere it does
    /// nothing.
    fn without_criterion(mut self) -> Self {
        let Focus::Criterion(index) = self.focus else {
            return self;
        };
        if index < self.criteria.len() {
            self.criteria.remove(index);
        }
        self.focus = match self.criteria.len().checked_sub(1) {
            Some(last) => Focus::Criterion(index.min(last)),
            None => Focus::Body,
        };
        self
    }

    /// The form after `key` was pressed in the field the focus is on. The kind is changed
    /// by Left, Right and Space.
    fn press(mut self, key: KeyCode) -> Self {
        match self.focus {
            Focus::Title => self.title = self.title.press(key),
            Focus::Links => self.links = self.links.press(key),
            Focus::Body => self.body = self.body.press(key),
            Focus::Criterion(index) => {
                if let Some(criterion) = self.criteria.get_mut(index) {
                    *criterion = criterion.clone().press(key);
                }
            }
            Focus::Kind => {
                if matches!(key, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) {
                    self.kind = match self.kind {
                        TaskKind::Agent => TaskKind::Human,
                        TaskKind::Human => TaskKind::Agent,
                    };
                }
            }
        }
        self
    }
}

/// What a key on the task form asks the rest of the application to do, when it is not
/// something the form answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the form without adding anything.
    Close,
    /// Add the task the form holds, at its placement; the form stays open until the loop's
    /// answer closes it or shows why it did not.
    Submit(TaskDraft, Placement),
    /// Leave every screen: the discard question was answered `y`.
    Quit,
}

/// The task form's own state: the task being written, and, after a first Ctrl-C finds
/// something typed, whether it is asking to discard it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskFormScreen {
    form: Form,
    discarding: bool,
    help: bool,
}

impl TaskFormScreen {
    /// An empty form open on its title, for a task that goes at `placement`.
    pub(crate) fn new(placement: Placement) -> Self {
        Self {
            form: Form::new(placement),
            discarding: false,
            help: false,
        }
    }

    /// Where the task the form holds goes in the queue.
    pub(crate) fn placement(&self) -> Placement {
        self.form.placement
    }

    /// Whether anything has been typed into the form: an empty task is nothing a discard
    /// would lose.
    pub(crate) fn has_content(&self) -> bool {
        self.form.has_content()
    }

    /// Whether the form is currently asking to discard its content.
    pub(crate) fn is_discarding(&self) -> bool {
        self.discarding
    }

    /// Why the form's last submission added nothing, when it was refused.
    #[cfg(test)]
    pub(crate) fn problems(&self) -> &[String] {
        &self.form.problems
    }

    /// The form asking to confirm discarding its content, after a first Ctrl-C found some.
    pub(crate) fn start_discard(self) -> Self {
        Self {
            discarding: true,
            help: false,
            ..self
        }
    }

    /// The form once its submission was refused, showing why; it stays open with what was
    /// typed.
    pub(crate) fn rejected(self, problems: Vec<String>) -> Self {
        Self {
            form: Form {
                problems,
                ..self.form
            },
            ..self
        }
    }

    /// A key on the task form: routed to the discard question while it is asking, or to the
    /// form's own fields otherwise.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        if self.discarding {
            return self.discard_key(key);
        }
        match key {
            KeyCode::Esc => (self, Some(Request::Close)),
            KeyCode::Tab => (
                Self {
                    form: self.form.moved(true),
                    ..self
                },
                None,
            ),
            KeyCode::BackTab => (
                Self {
                    form: self.form.moved(false),
                    ..self
                },
                None,
            ),
            _ => (
                Self {
                    form: self.form.press(key),
                    ..self
                },
                None,
            ),
        }
    }

    /// A key while the discard question is asking: while its own key map is open, only `?`
    /// or Esc, to close it back onto the question, answer.
    fn discard_key(self, key: KeyCode) -> (Self, Option<Request>) {
        if self.help {
            return match key {
                KeyCode::Esc | KeyCode::Char('?') => (
                    Self {
                        help: false,
                        ..self
                    },
                    None,
                ),
                _ => (self, None),
            };
        }
        match key {
            KeyCode::Char('y') => (self, Some(Request::Quit)),
            KeyCode::Char('n') | KeyCode::Esc => (
                Self {
                    discarding: false,
                    ..self
                },
                None,
            ),
            KeyCode::Char('?') => (Self { help: true, ..self }, None),
            _ => (self, None),
        }
    }

    /// A Ctrl-letter on the task form: ignored outright while the discard question is asking,
    /// since only `y`, `n`, Esc and `?` answer it.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        if self.discarding {
            return (self, None);
        }
        match letter {
            's' => {
                let request = Request::Submit(self.form.draft(), self.form.placement);
                (self, Some(request))
            }
            'n' => (
                Self {
                    form: self.form.with_criterion(),
                    ..self
                },
                None,
            ),
            'd' => (
                Self {
                    form: self.form.without_criterion(),
                    ..self
                },
                None,
            ),
            _ => (self, None),
        }
    }

    /// What the frame's bottom border says while the task form is showing.
    pub(crate) fn footer_keys(&self) -> &'static str {
        if self.discarding {
            DISCARD_KEYS
        } else {
            FORM_KEYS
        }
    }

    /// Draws the task form over the whole of `area`, and returns where the cursor goes.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if self.discarding && self.help {
            key_map(&DISCARD_QUESTION_KEYS, area, buf);
            return None;
        }
        draw_form(&self.form, self.discarding, area, buf)
    }
}

/// The rows of the form so far, and where the focus is in them.
struct Sheet {
    width: usize,
    rows: Vec<Line<'static>>,
    /// The row the focus is on, which the screen scrolls to keep in view.
    focus_row: usize,
    /// The column of the cursor in that row, when the focus is on text.
    cursor_x: Option<usize>,
}

impl Sheet {
    fn push(&mut self, text: String) {
        self.rows.push(Line::from(text));
    }

    /// Adds the rows of `area`, the first behind `first` and the others behind `rest`; when
    /// `focused` the cursor is on one of them, and a long line scrolls sideways to keep it
    /// in view.
    fn text(&mut self, first: &str, rest: &str, area: &TextArea, focused: bool) {
        let (cursor_row, cursor_col) = area.cursor();
        for (index, line) in area.lines().iter().enumerate() {
            let prefix = if index == 0 { first } else { rest };
            let mut shown: Vec<char> = line.chars().collect();
            if focused && index == cursor_row {
                let room = self.width.saturating_sub(prefix.chars().count()).max(1);
                let skip = (cursor_col + 1).saturating_sub(room);
                shown.drain(..skip.min(shown.len()));
                let before: String = line.chars().skip(skip).take(cursor_col - skip).collect();
                self.focus_row = self.rows.len();
                self.cursor_x = Some(Line::from(format!("{prefix}{before}")).width());
            }
            self.push(format!("{prefix}{}", shown.into_iter().collect::<String>()));
        }
    }
}

/// `text` broken into rows of at most `width` characters, at spaces where it can be.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let row = rows.last_mut().map_or(0, |row| row.chars().count());
            let separator = usize::from(row > 0);
            if row + separator + word.len() <= width {
                break;
            }
            if row > 0 {
                rows.push(String::new());
            } else {
                // A word longer than a row is cut.
                let rest = word.split_off(width.min(word.len()));
                if let Some(last) = rows.last_mut() {
                    last.extend(word);
                }
                rows.push(String::new());
                word = rest;
            }
        }
        if let Some(row) = rows.last_mut() {
            if !row.is_empty() {
                row.push(' ');
            }
            row.extend(word);
        }
    }
    rows
}

/// The marker in front of the field the focus is on.
fn marker(form: &Form, field: Focus) -> char {
    if form.focus == field { '>' } else { ' ' }
}

/// What the form is for, and where the task goes when it is not at the end.
fn heading(placement: Placement) -> String {
    match placement {
        Placement::End => "New task".to_owned(),
        Placement::Before(id) => format!("New task above #{id}"),
        Placement::After(id) => format!("New task below #{id}"),
    }
}

/// Pushes the discard notice, when `discard` asks for it, then every problem the form's last
/// submission found, each wrapped to `sheet`'s width.
fn push_problems(sheet: &mut Sheet, form: &Form, discard: bool, bold: Style) {
    if discard {
        sheet.rows.push(Line::styled(
            "Discard this task? y to discard · n or Esc to keep writing",
            bold,
        ));
    }
    for problem in &form.problems {
        for (index, row) in wrap(problem, sheet.width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if index == 0 { "! " } else { "  " };
            sheet.rows.push(Line::styled(format!("{lead}{row}"), bold));
        }
    }
}

/// Pushes the kind field: its value between angle brackets, with a hint on how to change it
/// while the focus is on it.
fn push_kind(sheet: &mut Sheet, form: &Form) {
    if form.focus == Focus::Kind {
        sheet.focus_row = sheet.rows.len();
    }
    let hint = if form.focus == Focus::Kind {
        "   Left, Right or Space to change"
    } else {
        ""
    };
    sheet.push(format!(
        "{} Kind:      < {} >{hint}",
        marker(form, Focus::Kind),
        form.kind
    ));
}

/// Pushes the criteria field: one row per criterion, or a hint when there are none yet.
fn push_criteria(sheet: &mut Sheet, form: &Form) {
    sheet.push("  Criteria:".to_owned());
    if form.criteria.is_empty() {
        sheet.push("    none: Ctrl-N adds one".to_owned());
    }
    for (index, criterion) in form.criteria.iter().enumerate() {
        let focus = Focus::Criterion(index);
        sheet.text(
            &format!("{}{:>3}. ", marker(form, focus), index + 1),
            "      ",
            criterion,
            form.focus == focus,
        );
    }
}

/// Pushes `form`'s own fields — title, kind, links, body and criteria — each marked with the
/// focus when it is on that field.
fn push_fields(sheet: &mut Sheet, form: &Form) {
    let mark = |field| marker(form, field);
    sheet.text(
        &format!("{} Title:     ", mark(Focus::Title)),
        "",
        &form.title,
        form.focus == Focus::Title,
    );
    push_kind(sheet, form);
    sheet.text(
        &format!("{} Links:     ", mark(Focus::Links)),
        "",
        &form.links,
        form.focus == Focus::Links,
    );
    sheet.push(format!("{} Body:", mark(Focus::Body)));
    sheet.text("    ", "    ", &form.body, form.focus == Focus::Body);
    push_criteria(sheet, form);
}

/// Draws `form` over the whole of `area`, asking to discard it when `discard` says so, and
/// returns where the cursor goes.
fn draw_form(form: &Form, discard: bool, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut sheet = Sheet {
        width: usize::from(area.width),
        rows: vec![Line::styled(heading(form.placement), bold)],
        focus_row: 0,
        cursor_x: None,
    };
    push_problems(&mut sheet, form, discard, bold);
    sheet.push(String::new());
    push_fields(&mut sheet, form);

    let height = usize::from(area.height);
    let first = (sheet.focus_row + 1).saturating_sub(height);
    let visible: Vec<Line<'static>> = sheet.rows.into_iter().skip(first).take(height).collect();
    Paragraph::new(visible).render(area, buf);
    let x = u16::try_from(sheet.cursor_x?)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(1));
    let y = u16::try_from(sheet.focus_row - first).unwrap_or(u16::MAX);
    Some(Position::new(area.x + x, area.y + y))
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;

    fn type_in(form: Form, text: &str) -> Form {
        text.chars().fold(form, |form, c| {
            form.press(if c == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(c)
            })
        })
    }

    #[test]
    fn a_new_form_is_empty_with_one_criterion_and_the_focus_on_the_title() {
        let form = Form::new(Placement::End);
        assert_eq!(form.focus, Focus::Title);
        assert_eq!(
            form.draft(),
            TaskDraft {
                title: String::new(),
                body: String::new(),
                criteria: vec![String::new()],
                kind: TaskKind::Agent,
                links: vec![],
            }
        );
    }

    #[test]
    fn the_draft_is_what_was_typed_with_links_split_on_spaces() {
        let form = type_in(Form::new(Placement::End), "Title");
        let form = type_in(form.moved(true).moved(true), "github:a/b#1  https://x.io ");
        let form = type_in(form.moved(true), "one\ntwo");
        let form = type_in(form.moved(true), "first more");
        let form = type_in(form.with_criterion(), "second");
        assert_eq!(
            form.draft(),
            TaskDraft {
                title: "Title".to_owned(),
                body: "one\ntwo".to_owned(),
                criteria: vec!["first more".to_owned(), "second".to_owned()],
                kind: TaskKind::Agent,
                links: vec!["github:a/b#1".to_owned(), "https://x.io".to_owned()],
            }
        );
    }

    #[test]
    fn a_criterion_has_one_line_and_ignores_enter() {
        let form = type_in(Form::new(Placement::End).moved(false), "a\nb");
        assert_eq!(form.draft().criteria, ["ab"]);
    }

    #[test]
    fn the_focus_walks_the_fields_in_order_and_wraps_both_ways() {
        let form = Form::new(Placement::End).with_criterion();
        let mut seen = vec![Focus::Criterion(1)];
        let mut walking = form.clone().moved(true);
        while walking.focus != Focus::Criterion(1) {
            seen.push(walking.focus);
            walking = walking.moved(true);
        }
        assert_eq!(
            seen,
            [
                Focus::Criterion(1),
                Focus::Title,
                Focus::Kind,
                Focus::Links,
                Focus::Body,
                Focus::Criterion(0)
            ]
        );
        let back = Form::new(Placement::End).moved(false);
        assert_eq!(back.focus, Focus::Criterion(0));
        assert_eq!(back.moved(false).focus, Focus::Body);
    }

    #[test]
    fn left_right_and_space_toggle_the_kind_and_other_keys_do_not() {
        let form = Form::new(Placement::End).moved(true);
        assert_eq!(form.focus, Focus::Kind);
        for key in [KeyCode::Left, KeyCode::Right, KeyCode::Char(' ')] {
            let human = form.clone().press(key);
            assert_eq!(human.kind, TaskKind::Human);
            assert_eq!(human.press(key).kind, TaskKind::Agent);
        }
        for key in [KeyCode::Char('h'), KeyCode::Enter, KeyCode::Backspace] {
            assert_eq!(form.clone().press(key), form);
        }
    }

    #[test]
    fn a_criterion_is_added_after_the_last_with_the_focus_on_it() {
        let form = Form::new(Placement::End).with_criterion().with_criterion();
        assert_eq!(form.criteria.len(), 3);
        assert_eq!(form.focus, Focus::Criterion(2));
    }

    #[test]
    fn removing_a_criterion_moves_the_focus_to_its_neighbour_then_to_the_body() {
        let form = type_in(Form::new(Placement::End).moved(false), "a");
        let form = type_in(form.with_criterion(), "b");
        let form = type_in(form.with_criterion(), "c");
        let form = form.moved(false).without_criterion();
        assert_eq!(form.focus, Focus::Criterion(1));
        assert_eq!(form.draft().criteria, ["a", "c"]);
        let form = form.without_criterion();
        assert_eq!(form.focus, Focus::Criterion(0));
        assert_eq!(form.draft().criteria, ["a"]);
        let form = form.without_criterion();
        assert_eq!(form.focus, Focus::Body);
        assert!(form.criteria.is_empty());
        assert_eq!(form.clone().without_criterion(), form);
    }

    #[test]
    fn with_no_criterion_left_the_focus_still_walks_and_one_can_be_added() {
        let form = Form::new(Placement::End).moved(false).without_criterion();
        assert_eq!(form.clone().moved(true).focus, Focus::Title);
        assert_eq!(form.moved(false).focus, Focus::Links);
        let form = Form::new(Placement::End)
            .moved(false)
            .without_criterion()
            .with_criterion();
        assert_eq!(form.focus, Focus::Criterion(0));
    }

    #[test]
    fn wrap_breaks_text_at_spaces_and_no_row_is_longer_than_the_width() {
        assert_eq!(wrap("a short one", 11), ["a short one"]);
        assert_eq!(wrap("aaa bbb ccc dd", 7), ["aaa bbb", "ccc dd"]);
        assert_eq!(wrap("abcdefgh x", 3), ["abc", "def", "gh", "x"]);
        assert_eq!(wrap("", 5), [""]);
    }

    fn press(screen: TaskFormScreen, keys: &[KeyCode]) -> TaskFormScreen {
        keys.iter().fold(screen, |screen, key| screen.key(*key).0)
    }

    fn typed(screen: TaskFormScreen, text: &str) -> TaskFormScreen {
        text.chars().fold(screen, |screen, c| {
            screen
                .key(if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                })
                .0
        })
    }

    #[test]
    fn esc_asks_to_close_the_form() {
        let screen = TaskFormScreen::new(Placement::End);
        let (_, request) = screen.key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn typing_edits_the_title_and_the_focus_walks_with_tab() {
        let screen = typed(TaskFormScreen::new(Placement::End), "qjdna?");
        assert_eq!(screen.form.draft().title, "qjdna?");
        let screen = press(screen, &[KeyCode::Tab]);
        assert_eq!(screen.form.focus, Focus::Kind);
        let screen = press(screen, &[KeyCode::BackTab, KeyCode::BackTab]);
        assert_eq!(screen.form.focus, Focus::Criterion(0));
    }

    #[test]
    fn ctrl_n_and_ctrl_d_add_and_remove_criteria() {
        let screen = TaskFormScreen::new(Placement::End);
        let (screen, request) = screen.ctrl('n');
        assert_eq!(request, None);
        assert_eq!(screen.form.criteria.len(), 2);
        let (screen, _) = screen.ctrl('d');
        let (screen, _) = screen.ctrl('d');
        assert!(screen.form.criteria.is_empty());
        let (screen, request) = screen.ctrl('x');
        assert_eq!(request, None);
        assert!(screen.form.criteria.is_empty());
    }

    #[test]
    fn ctrl_s_submits_the_typed_task_and_leaves_the_form_open() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title");
        let (screen, request) = screen.ctrl('s');
        assert_eq!(
            request,
            Some(Request::Submit(screen.form.draft(), Placement::End))
        );
    }

    #[test]
    fn rejected_shows_the_problems_and_keeps_what_was_typed() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title");
        let screen = screen.rejected(vec!["a problem".to_owned()]);
        assert_eq!(screen.form.problems, ["a problem"]);
        assert_eq!(screen.form.draft().title, "Title");
    }

    #[test]
    fn o_and_capital_o_placements_are_reported_faithfully() {
        let screen = TaskFormScreen::new(Placement::After(ktask_core::TaskId(2)));
        assert_eq!(screen.placement(), Placement::After(ktask_core::TaskId(2)));
    }

    #[test]
    fn has_content_is_false_for_an_empty_form_and_true_once_something_is_typed() {
        assert!(!TaskFormScreen::new(Placement::End).has_content());
        assert!(typed(TaskFormScreen::new(Placement::End), "x").has_content());
    }

    #[test]
    fn start_discard_opens_the_question_and_only_its_answers_and_the_key_map_act() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title");
        let asked = screen.start_discard();
        assert!(asked.is_discarding());

        for key in [KeyCode::Char('q'), KeyCode::Char('x'), KeyCode::Enter] {
            let (still, request) = asked.clone().key(key);
            assert_eq!(still, asked);
            assert_eq!(request, None);
        }
        for letter in ['s', 'n', 'd'] {
            let (still, request) = asked.clone().ctrl(letter);
            assert_eq!(still, asked);
            assert_eq!(request, None);
        }

        let (_, request) = asked.clone().key(KeyCode::Char('y'));
        assert_eq!(request, Some(Request::Quit));

        for answer in [KeyCode::Char('n'), KeyCode::Esc] {
            let (back, request) = asked.clone().key(answer);
            assert!(!back.is_discarding());
            assert_eq!(request, None);
            assert_eq!(back.form.draft().title, "Title");
        }
    }

    #[test]
    fn while_discarding_the_key_map_opens_with_question_mark_and_closes_with_esc_or_it() {
        let asked = typed(TaskFormScreen::new(Placement::End), "Title").start_discard();
        let (mapped, request) = asked.clone().key(KeyCode::Char('?'));
        assert!(mapped.help);
        assert_eq!(request, None);
        for key in [KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Char('x')] {
            let (still, request) = mapped.clone().key(key);
            assert_eq!(still, mapped);
            assert_eq!(request, None);
        }
        for close in [KeyCode::Esc, KeyCode::Char('?')] {
            let (closed, _) = mapped.clone().key(close);
            assert!(!closed.help);
            assert!(closed.is_discarding());
        }
    }

    fn draw_screen(
        screen: &TaskFormScreen,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Option<Position>) {
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
    fn the_form_shows_its_fields_and_puts_the_cursor_in_the_title() {
        let screen = TaskFormScreen::new(Placement::End);
        let (rows, cursor) = draw_screen(&screen, 60, 12);
        assert_eq!(row(&rows, 0), "New task");
        assert_eq!(row(&rows, 2), "> Title:");
        assert_eq!(row(&rows, 3), "  Kind:      < agent >");
        assert_eq!(row(&rows, 4), "  Links:");
        assert_eq!(row(&rows, 5), "  Body:");
        assert_eq!(row(&rows, 7), "  Criteria:");
        assert_eq!(cursor, Some(Position::new(13, 2)));
    }

    #[test]
    fn a_long_title_scrolls_sideways_so_that_the_cursor_stays_on_the_screen() {
        let screen = typed(TaskFormScreen::new(Placement::End), &"x".repeat(70));
        let (rows, cursor) = draw_screen(&screen, 60, 12);
        let cursor = cursor.expect("the cursor is shown");
        assert!(cursor.x < 60, "{cursor:?}");
        assert!(row(&rows, 2).ends_with('x'), "{rows:?}");
    }

    #[test]
    fn discarding_shows_the_question_and_the_typed_title_stays_visible() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title").start_discard();
        let (rows, _) = draw_screen(&screen, 60, 12);
        assert_eq!(
            row(&rows, 1),
            "Discard this task? y to discard · n or Esc to keep writing"
        );
        assert!(row(&rows, 3).ends_with("Title"), "{rows:?}");
    }

    #[test]
    fn discarding_with_help_open_shows_the_discard_questions_own_key_map() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title")
            .start_discard()
            .key(KeyCode::Char('?'))
            .0;
        let (rows, cursor) = draw_screen(&screen, 60, 12);
        let screen_text = rows.join("\n");
        assert!(screen_text.contains("discard it"), "{screen_text}");
        assert!(!screen_text.contains("Title"), "{screen_text}");
        assert_eq!(cursor, None);
    }
}

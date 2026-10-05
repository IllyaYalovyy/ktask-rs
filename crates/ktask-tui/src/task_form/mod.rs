//! The task form: a new task's own fields, where the focus is, a question to discard it
//! after a first Ctrl-C finds something typed, and how it is drawn. Covers the queue while it
//! is open.

use ktask_core::{Placement, TaskDraft, TaskKind};
use ratatui::crossterm::event::KeyCode;

use crate::text::TextArea;

mod render;

/// What the form's frame says at the bottom: the keys that are not typing.
const FORM_KEYS: &str = " Ctrl-S add · Esc cancel · Tab, Shift-Tab field · Ctrl-N, Ctrl-D criterion · Ctrl-P provider · Ctrl-O model ";

/// What the form's frame says at the bottom while it asks to discard the task.
const DISCARD_KEYS: &str = " y discard · n, Esc keep writing ";

/// The field the keys go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Title,
    Kind,
    Provider,
    Model,
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
    provider: TextArea,
    model: TextArea,
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
            provider: TextArea::new(false),
            model: TextArea::new(false),
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
            || !self.provider.text().is_empty()
            || !self.model.text().is_empty()
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
            provider: (!self.provider.text().is_empty()).then(|| self.provider.text()),
            model: (!self.model.text().is_empty()).then(|| self.model.text()),
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
            Focus::Provider => self.provider = self.provider.press(key),
            Focus::Model => self.model = self.model.press(key),
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
            'p' => (
                Self {
                    form: Form {
                        focus: Focus::Provider,
                        ..self.form
                    },
                    ..self
                },
                None,
            ),
            'o' => (
                Self {
                    form: Form {
                        focus: Focus::Model,
                        ..self.form
                    },
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
}

#[cfg(test)]
mod tests {
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
                provider: None,
                model: None,
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
                provider: None,
                model: None,
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
}

//! The form a new task is written in: its fields, where the focus is, and what it holds.

use ktask_core::{TaskDraft, TaskKind};
use ratatui::crossterm::event::KeyCode;

use crate::text::TextArea;

/// The field the keys go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Title,
    Kind,
    Links,
    Body,
    /// The criterion at this index.
    Criterion(usize),
}

/// A task being written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Form {
    pub(crate) title: TextArea,
    pub(crate) kind: TaskKind,
    /// Links are written on one line, apart by spaces: a link holds no space.
    pub(crate) links: TextArea,
    pub(crate) body: TextArea,
    pub(crate) criteria: Vec<TextArea>,
    pub(crate) focus: Focus,
    /// Why the last submission added nothing.
    pub(crate) problems: Vec<String>,
}

impl Form {
    /// An empty form with one empty criterion, on its title.
    pub(crate) fn new() -> Self {
        Self {
            title: TextArea::new(false),
            kind: TaskKind::default(),
            links: TextArea::new(false),
            body: TextArea::new(true),
            criteria: vec![TextArea::new(true)],
            focus: Focus::Title,
            problems: Vec::new(),
        }
    }

    /// The task the form holds, as typed.
    pub(crate) fn draft(&self) -> TaskDraft {
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
    pub(crate) fn moved(self, forward: bool) -> Self {
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
    pub(crate) fn with_criterion(mut self) -> Self {
        self.criteria.push(TextArea::new(true));
        self.focus = Focus::Criterion(self.criteria.len() - 1);
        self
    }

    /// The form without the criterion the focus is on; the focus goes to the one that takes
    /// its place, or the one before it, or the body when none is left. Elsewhere it does
    /// nothing.
    pub(crate) fn without_criterion(mut self) -> Self {
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
    pub(crate) fn press(mut self, key: KeyCode) -> Self {
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
        let form = Form::new();
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
        let form = type_in(Form::new(), "Title");
        let form = type_in(form.moved(true).moved(true), "github:a/b#1  https://x.io ");
        let form = type_in(form.moved(true), "one\ntwo");
        let form = type_in(form.moved(true), "first\nmore");
        let form = type_in(form.with_criterion(), "second");
        assert_eq!(
            form.draft(),
            TaskDraft {
                title: "Title".to_owned(),
                body: "one\ntwo".to_owned(),
                criteria: vec!["first\nmore".to_owned(), "second".to_owned()],
                kind: TaskKind::Agent,
                links: vec!["github:a/b#1".to_owned(), "https://x.io".to_owned()],
            }
        );
    }

    #[test]
    fn the_focus_walks_the_fields_in_order_and_wraps_both_ways() {
        let form = Form::new().with_criterion();
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
        let back = Form::new().moved(false);
        assert_eq!(back.focus, Focus::Criterion(0));
        assert_eq!(back.moved(false).focus, Focus::Body);
    }

    #[test]
    fn left_right_and_space_toggle_the_kind_and_other_keys_do_not() {
        let form = Form::new().moved(true);
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
        let form = Form::new().with_criterion().with_criterion();
        assert_eq!(form.criteria.len(), 3);
        assert_eq!(form.focus, Focus::Criterion(2));
    }

    #[test]
    fn removing_a_criterion_moves_the_focus_to_its_neighbour_then_to_the_body() {
        let form = type_in(Form::new().moved(false), "a");
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
        let form = Form::new().moved(false).without_criterion();
        assert_eq!(form.clone().moved(true).focus, Focus::Title);
        assert_eq!(form.moved(false).focus, Focus::Links);
        let form = Form::new()
            .moved(false)
            .without_criterion()
            .with_criterion();
        assert_eq!(form.focus, Focus::Criterion(0));
    }
}

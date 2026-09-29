//! The form that asks for the path of the file to import tasks from.

use ratatui::crossterm::event::KeyCode;

use crate::text::TextArea;

/// The file path being typed, before an import is attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportForm {
    pub(crate) path: TextArea,
}

impl ImportForm {
    /// An empty form.
    pub(crate) fn new() -> Self {
        Self {
            path: TextArea::new(false),
        }
    }

    /// The form after `key` was pressed in the path field.
    pub(crate) fn press(mut self, key: KeyCode) -> Self {
        self.path = self.path.press(key);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_form_has_an_empty_path() {
        assert_eq!(ImportForm::new().path.text(), "");
    }

    #[test]
    fn typing_edits_the_path() {
        let form = "/tmp/tasks.json"
            .chars()
            .fold(ImportForm::new(), |form, c| form.press(KeyCode::Char(c)));
        assert_eq!(form.path.text(), "/tmp/tasks.json");
    }
}

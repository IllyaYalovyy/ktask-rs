//! The settings screen: the project's attempt-timeout setting, and the field it is edited
//! in.

use ktask_core::SettingView;
use ratatui::crossterm::event::KeyCode;

use crate::text::TextArea;

/// The settings screen, open on the attempt-timeout field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsForm {
    /// Whether the value on show, before anything is typed, was the built-in default rather
    /// than one the project had set.
    pub(crate) is_default: bool,
    /// The attempt-timeout field, as seconds typed so far.
    pub(crate) attempt_timeout: TextArea,
    /// Why the last submission changed nothing.
    pub(crate) problem: Option<String>,
}

impl SettingsForm {
    /// A settings screen opened on `view`'s current value.
    pub(crate) fn new(view: &SettingView) -> Self {
        Self {
            is_default: view.is_default,
            attempt_timeout: TextArea::with_text(false, &view.value.to_string()),
            problem: None,
        }
    }

    /// The attempt-timeout field after `key` was pressed in it.
    pub(crate) fn press(mut self, key: KeyCode) -> Self {
        self.attempt_timeout = self.attempt_timeout.press(key);
        self
    }

    /// The value as typed, for the loop to submit.
    pub(crate) fn value(&self) -> String {
        self.attempt_timeout.text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(value: u64, is_default: bool) -> SettingView {
        SettingView {
            name: "attempt-timeout",
            value,
            is_default,
        }
    }

    #[test]
    fn a_new_screen_opens_with_the_current_value_and_no_problem() {
        let form = SettingsForm::new(&view(14_400, true));
        assert_eq!(form.value(), "14400");
        assert!(form.is_default);
        assert_eq!(form.problem, None);
    }

    #[test]
    fn typing_changes_the_value() {
        let form = SettingsForm::new(&view(14_400, true));
        let form = "600"
            .chars()
            .fold(form, |form, c| form.press(KeyCode::Char(c)));
        assert_eq!(form.value(), "14400600");
        let form = form.press(KeyCode::Backspace);
        assert_eq!(form.value(), "1440060");
    }
}

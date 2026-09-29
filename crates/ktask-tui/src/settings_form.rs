//! The settings screen: every project setting, and the field each is edited in.

use ktask_core::SettingView;
use ratatui::crossterm::event::KeyCode;

use crate::text::TextArea;

/// One setting's own field in the settings screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingField {
    /// The setting's name, submitted alongside its value.
    pub(crate) name: &'static str,
    /// Whether the value on show, before anything is typed, was the built-in default rather
    /// than one the project had set.
    pub(crate) is_default: bool,
    /// The value, as typed so far.
    pub(crate) text: TextArea,
}

/// The settings screen: one field per project setting, with the focus on one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsForm {
    /// Every setting's field, in the order `settings` lists them.
    pub(crate) fields: Vec<SettingField>,
    /// The index into `fields` the keys go to.
    pub(crate) focus: usize,
    /// Why the last submission changed nothing.
    pub(crate) problem: Option<String>,
}

impl SettingsForm {
    /// A settings screen opened on `views`' current values, the focus on the first.
    pub(crate) fn new(views: &[SettingView]) -> Self {
        Self {
            fields: views
                .iter()
                .map(|view| SettingField {
                    name: view.name,
                    is_default: view.is_default,
                    text: TextArea::with_text(false, &view.value),
                })
                .collect(),
            focus: 0,
            problem: None,
        }
    }

    /// The focused field after `key` was pressed in it.
    pub(crate) fn press(mut self, key: KeyCode) -> Self {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.text = field.text.clone().press(key);
        }
        self
    }

    /// The screen with the focus on the next field, or the previous one; it wraps around.
    pub(crate) fn moved(mut self, forward: bool) -> Self {
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

    /// The focused field's value, for the loop to submit.
    pub(crate) fn value(&self) -> String {
        self.fields
            .get(self.focus)
            .map(|field| field.text.text())
            .unwrap_or_default()
    }

    /// The focused field's setting name, for the loop to submit the value against.
    pub(crate) fn name(&self) -> Option<&'static str> {
        self.fields.get(self.focus).map(|field| field.name)
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_new_screen_opens_on_the_first_field_with_its_value_and_no_problem() {
        let form = SettingsForm::new(&views());
        assert_eq!(form.focus, 0);
        assert_eq!(form.name(), Some("attempt-timeout"));
        assert_eq!(form.value(), "14400");
        assert!(form.fields[0].is_default);
        assert_eq!(form.problem, None);
    }

    #[test]
    fn typing_changes_the_focused_fields_value_only() {
        let form = SettingsForm::new(&views());
        let form = "600"
            .chars()
            .fold(form, |form, c| form.press(KeyCode::Char(c)));
        assert_eq!(form.value(), "14400600");
        let form = form.press(KeyCode::Backspace);
        assert_eq!(form.value(), "1440060");
        assert_eq!(form.fields[1].text.text(), "");
    }

    #[test]
    fn moved_walks_the_fields_and_wraps_both_ways() {
        let form = SettingsForm::new(&views());
        assert_eq!(form.name(), Some("attempt-timeout"));
        let form = form.moved(true);
        assert_eq!(form.name(), Some("health-check"));
        let form = form.moved(true);
        assert_eq!(form.name(), Some("attempt-timeout"));
        let form = form.moved(false);
        assert_eq!(form.name(), Some("health-check"));
    }

    #[test]
    fn typing_after_moving_edits_the_newly_focused_field() {
        let form = SettingsForm::new(&views()).moved(true);
        let form = "cargo test"
            .chars()
            .fold(form, |form, c| form.press(KeyCode::Char(c)));
        assert_eq!(form.value(), "cargo test");
        assert_eq!(form.fields[0].text.text(), "14400");
    }
}

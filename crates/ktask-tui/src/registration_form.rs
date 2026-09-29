//! The form that asks for a name to register the current directory under, after its folder
//! name turned out to already be taken by another registered project.

use ratatui::crossterm::event::KeyCode;

use crate::text::TextArea;

/// The name being typed to register the directory under, and why a name is needed at all — the
/// refusal that opened this screen, or, after a name it was submitted with was refused too, the
/// same words `ktask-rs project register --name` gives for the same conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistrationForm {
    pub(crate) problem: String,
    pub(crate) name: TextArea,
}

impl RegistrationForm {
    /// A form open on `problem`, with an empty name.
    pub(crate) fn new(problem: String) -> Self {
        Self {
            problem,
            name: TextArea::new(false),
        }
    }

    /// The form after `key` was pressed in the name field.
    pub(crate) fn press(mut self, key: KeyCode) -> Self {
        self.name = self.name.press(key);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_form_holds_the_problem_it_opened_on_with_an_empty_name() {
        let form = RegistrationForm::new("conflict".to_owned());
        assert_eq!(form.problem, "conflict");
        assert_eq!(form.name.text(), "");
    }

    #[test]
    fn typing_edits_the_name() {
        let form = "my-app"
            .chars()
            .fold(RegistrationForm::new("conflict".to_owned()), |form, c| {
                form.press(KeyCode::Char(c))
            });
        assert_eq!(form.name.text(), "my-app");
    }
}

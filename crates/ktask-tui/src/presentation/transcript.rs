//! How the transcripts of an attempt are laid out for an operator: one section per agent step,
//! each beginning with a line that says who spoke.

use ktask_core::StepTranscript;

/// The sections of one attempt's output, in the order the steps ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    text: String,
    heading_lines: Vec<usize>,
}

/// The line that begins a step's transcript: the step, its provider and its model.
#[must_use]
pub fn step_heading(step: &StepTranscript) -> String {
    match &step.model {
        Some(model) => format!("--- {} · {} · {model} ---", step.step, step.provider),
        None => format!("--- {} · {} ---", step.step, step.provider),
    }
}

impl Transcript {
    /// Lays out `steps`, each under its heading, with a blank line between them.
    #[must_use]
    pub fn new(steps: &[StepTranscript]) -> Self {
        let mut text = String::new();
        let mut heading_lines = Vec::with_capacity(steps.len());
        for step in steps {
            if !text.is_empty() {
                text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
            }
            heading_lines.push(text.matches('\n').count());
            text.push_str(&step_heading(step));
            text.push('\n');
            text.push_str(&step.readable());
        }
        Self {
            text,
            heading_lines,
        }
    }

    /// Everything to show, headings included.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many steps have a section.
    #[must_use]
    pub fn steps(&self) -> usize {
        self.heading_lines.len()
    }

    /// The line, counted from the top of [`Self::text`], on which step `index`'s section begins.
    #[must_use]
    pub fn heading_line(&self, index: usize) -> Option<usize> {
        self.heading_lines.get(index).copied()
    }
}

#[cfg(test)]
mod tests {
    use ktask_core::ProviderParser;

    use super::*;

    fn step(name: &str, model: Option<&str>, said: &str) -> StepTranscript {
        StepTranscript::new(name, "echo", model, ProviderParser::Plain, said.as_bytes())
    }

    #[test]
    fn a_heading_names_the_step_the_provider_and_the_model() {
        assert_eq!(
            step_heading(&step("review", Some("m1"), "")),
            "--- review · echo · m1 ---"
        );
        assert_eq!(
            step_heading(&step("review", None, "")),
            "--- review · echo ---"
        );
    }

    #[test]
    fn sections_follow_in_order_one_blank_line_apart_and_know_where_they_begin() {
        let transcript = Transcript::new(&[
            step("implementation", None, "one\ntwo\n"),
            step("review", None, "three"),
            step("testing", None, ""),
        ]);

        assert_eq!(
            transcript.text(),
            "--- implementation · echo ---\none\ntwo\n\n--- review · echo ---\nthree\n\n--- testing · echo ---\n"
        );
        assert_eq!(transcript.steps(), 3);
        assert_eq!(
            [0, 1, 2, 3].map(|index| transcript.heading_line(index)),
            [Some(0), Some(4), Some(7), None]
        );
    }

    #[test]
    fn no_steps_is_no_text() {
        assert_eq!(Transcript::new(&[]).text(), "");
    }
}

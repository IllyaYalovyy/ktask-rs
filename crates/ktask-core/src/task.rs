//! The task as the queue holds it in memory: what was asked, and how it is
//! proved.
//!
//! A task is the text an operator authored, imported once and kept verbatim.
//! `body` is the block as written so the original document can be shown again
//! on the Task detail screen; the four named fields are the sections
//! `.ktask/README.md` requires of every task, held separately because the
//! queue stores them as separate columns and the UI reads them separately.
//!
//! Nothing here decides whether a task is finished. [`TaskStatus`] says what the
//! supervisor concluded, and that conclusion comes from the journal, never from
//! this struct — an agent's own claim is not evidence (VISION.md §2).
//!
//! The state machine that drives a task through preflight, attempts and gates is
//! `TaskState` in `state.rs`, a different and much richer type. This is the
//! queue entry; that is the run.

use crate::ids::TaskId;

/// What the queue holds a task for, at the level the list screen shows.
///
/// The five states an operator acts on. The intermediate states of a run —
/// preflight, an attempt in progress, publishing — belong to
/// `TaskState` and are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Queued and not yet run.
    Pending,
    /// Verified and published.
    Done,
    /// Terminally failed; the run stopped here.
    Failed,
    /// Paused on a question only a human can answer.
    NeedsInput,
    /// Paused at a gate that waits for a human decision.
    HumanGate,
}

/// A task in the queue: its text, and what the supervisor concluded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Its position in queue order.
    pub id: TaskId,
    /// What the supervisor concluded about it.
    pub status: TaskStatus,
    /// The task block as authored, headings and all.
    pub body: String,
    /// The `Outcome:` section — the change a reader should be able to name.
    pub outcome: String,
    /// The `Done-when:` section — the observable fact that finishes the task.
    pub done_when: String,
    /// The `Verify:` section — the command that proves it mechanically.
    pub verify: String,
    /// The `Refs:` section — the documents the task is answerable to.
    pub refs: String,
}

/// How many characters [`Task::title`] keeps before it cuts the line.
///
/// A count of characters rather than bytes, so the cut is the same for a task
/// written in any script: 80 Latin letters and 80 ideographs each fill the same
/// slot.
const TITLE_MAX_CHARS: usize = 80;

impl Task {
    /// The first line of [`Task::body`], cut to 80 characters.
    ///
    /// This is what the queue list and `status` print, and it is a projection
    /// rather than stored text: the body is the authority, so a title can
    /// never disagree with the task it names.
    ///
    /// The cut is at a character boundary, never inside a multi-byte
    /// character — a queue that prints half a character prints mojibake, and
    /// the reference is borrowed from `body` so there is no room to repair it
    /// afterwards. Nothing is added to mark the cut: an ellipsis would be one
    /// more character in a space measured to the character, and the truncation
    /// is the interface's business (see ADR-0006).
    #[must_use]
    pub fn title(&self) -> &str {
        let first_line = self.body.lines().next().unwrap_or_default();
        match first_line.char_indices().nth(TITLE_MAX_CHARS) {
            // The character at the limit exists, so the line is longer than
            // the limit and `offset` is the byte index it starts at.
            Some((offset, _)) => first_line.get(..offset).unwrap_or(first_line),
            None => first_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Task, TaskStatus};
    use crate::ids::TaskId;

    /// A task whose first line is `first_line` and whose rest is ordinary.
    fn task_with_first_line(first_line: &str) -> Task {
        Task {
            id: TaskId::new(7),
            status: TaskStatus::Pending,
            body: format!("{first_line}\n\n**Outcome:** the queue holds a task.\n"),
            outcome: "the queue holds a task".to_owned(),
            done_when: "a test asserts it".to_owned(),
            verify: "cargo test".to_owned(),
            refs: "docs/DESIGN.md".to_owned(),
        }
    }

    #[test]
    fn a_title_is_the_first_line_of_the_body_and_nothing_below_it() {
        let task = task_with_first_line("Give the queue a task to hold");
        assert_eq!(task.title(), "Give the queue a task to hold");
    }

    #[test]
    fn an_empty_body_has_an_empty_title() {
        let mut task = task_with_first_line("unused");
        task.body = String::new();
        assert_eq!(task.title(), "");
    }

    #[test]
    fn a_body_opening_with_a_line_break_has_an_empty_title() {
        let mut task = task_with_first_line("unused");
        task.body = "\nthe first line is empty".to_owned();
        assert_eq!(task.title(), "");
    }

    #[test]
    fn a_title_stops_at_the_end_of_the_line_and_does_not_carry_the_break() {
        let mut task = task_with_first_line("unused");
        task.body = "first\r\nsecond".to_owned();
        assert_eq!(task.title(), "first");
    }

    #[test]
    fn a_title_of_exactly_the_limit_is_returned_whole() {
        let line = "a".repeat(80);
        let task = task_with_first_line(&line);
        assert_eq!(task.title(), line);
    }

    #[test]
    fn a_title_one_character_past_the_limit_loses_exactly_that_character() {
        let line = "a".repeat(80) + "b";
        let task = task_with_first_line(&line);
        assert_eq!(task.title(), "a".repeat(80));
        assert_eq!(task.title().chars().count(), 80);
    }

    #[test]
    fn a_long_title_is_cut_at_the_limit_and_the_rest_of_the_line_is_dropped() {
        let line = "Journal every transition before its side effect so that an interruption resolves to a known state rather than an ambiguous one";
        let task = task_with_first_line(line);
        let title = task.title();
        let kept: String = line.chars().take(80).collect();
        let dropped: String = line.chars().skip(80).collect();
        assert_eq!(title, kept);
        assert!(!title.is_empty());
        assert!(dropped.chars().count() > 0, "the fixture is over the limit");
        assert_eq!(format!("{title}{dropped}"), line);
    }

    #[test]
    fn a_cut_landing_inside_a_two_byte_character_keeps_the_whole_character() {
        // 79 ASCII characters then a two-byte 'é' straddling the 80th position:
        // a byte-counting cut at 80 would split it.
        let line = "a".repeat(79) + "é done";
        let task = task_with_first_line(&line);
        assert_eq!(task.title(), "a".repeat(79) + "é");
        assert_eq!(task.title().chars().count(), 80);
        assert_eq!(task.title().len(), 81, "the é is kept whole, not split");
    }

    #[test]
    fn a_cut_landing_inside_a_four_byte_character_keeps_the_whole_character() {
        // 79 ASCII characters then a four-byte emoji: a byte-counting cut at 80
        // would leave two of its four bytes behind.
        let line = format!("{}😀 rest of the line", "b".repeat(79));
        let task = task_with_first_line(&line);
        let expected: String = line.chars().take(80).collect();
        let dropped: String = line.chars().skip(80).collect();
        assert_eq!(task.title(), expected);
        assert_eq!(task.title().chars().count(), 80);
        assert_eq!(task.title().len(), 83, "the emoji is kept whole, not split");
        assert_eq!(format!("{}{}", task.title(), dropped), line);
        assert!(task.title().is_char_boundary(task.title().len()));
    }

    #[test]
    fn a_line_made_entirely_of_multibyte_characters_is_cut_at_a_character_boundary() {
        let line = "日本語のタイトル".repeat(20);
        let task = task_with_first_line(&line);
        let title = task.title();
        assert_eq!(title.chars().count(), 80);
        assert_eq!(title.len(), 240, "every kept character is three bytes");
        assert!(title.is_char_boundary(title.len()));
        assert_eq!(title, line.chars().take(80).collect::<String>());
    }
}

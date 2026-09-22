//! The in-memory task model: `Task` and `TaskStatus`.
//!
//! A `Task` is the unit of work a run supervises. Its fields mirror the
//! Markdown task format described in `.ktask/README.md`: `outcome`,
//! `done_when`, `verify` and `refs` come directly from the `**Outcome:**`,
//! `**Done-when:**`, `**Verify:**` and `**Refs:**` sections of a task, while
//! `body` holds the task's full Markdown text.

use crate::TaskId;

/// The maximum number of characters kept in a task's [`Task::title`].
const TITLE_MAX_CHARS: usize = 80;

/// Where a task stands in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    /// Queued but not yet started.
    Pending,
    /// Completed and verified.
    Done,
    /// Failed and will not be retried automatically.
    Failed,
    /// Blocked on information only a human can supply.
    NeedsInput,
    /// Blocked on a human's explicit approval to proceed.
    HumanGate,
}

/// A unit of work a run supervises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The task's position in the queue.
    pub id: TaskId,
    /// Where the task stands in its lifecycle.
    pub status: TaskStatus,
    /// The task's full Markdown text, as authored.
    pub body: String,
    /// The `**Outcome:**` section: what the task should achieve.
    pub outcome: String,
    /// The `**Done-when:**` section: the observable completion criteria.
    pub done_when: String,
    /// The `**Verify:**` section: the command that checks completion.
    pub verify: String,
    /// The `**Refs:**` section: pointers to supporting documentation.
    pub refs: String,
}

impl Task {
    /// The task's title: its body's first line, truncated to at most 80
    /// characters.
    ///
    /// The truncation point is always a character boundary, so a multi-byte
    /// character is never split.
    #[must_use]
    pub fn title(&self) -> &str {
        let first_line = self.body.lines().next().unwrap_or("");
        match first_line.char_indices().nth(TITLE_MAX_CHARS) {
            Some((byte_index, _)) => &first_line[..byte_index],
            None => first_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_body(body: &str) -> Task {
        Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: body.to_string(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        }
    }

    #[test]
    fn title_is_the_first_line() {
        let task = task_with_body("First line\nSecond line");
        assert_eq!(task.title(), "First line");
    }

    #[test]
    fn title_truncates_a_long_first_line_to_eighty_characters() {
        let long_line = "x".repeat(120);
        let task = task_with_body(&long_line);
        let title = task.title();
        assert_eq!(title.chars().count(), 80);
        assert_eq!(title, "x".repeat(80));
    }

    #[test]
    fn title_truncation_never_splits_a_multi_byte_character() {
        // Each "é" is a two-byte UTF-8 character; 80 of them land the naive
        // 80-*byte* cut in the middle of the 40th character.
        let long_line = "é".repeat(90);
        let task = task_with_body(&long_line);
        let title = task.title();

        // The result must itself be valid UTF-8 text (guaranteed by `&str`
        // slicing succeeding at all) and must contain exactly 80 characters,
        // not 80 bytes' worth of a character split in half.
        assert_eq!(title.chars().count(), 80);
        assert_eq!(title, "é".repeat(80));
    }

    #[test]
    fn title_of_a_short_body_is_unchanged() {
        let task = task_with_body("short");
        assert_eq!(task.title(), "short");
    }

    #[test]
    fn title_of_an_empty_body_is_empty() {
        let task = task_with_body("");
        assert_eq!(task.title(), "");
    }
}

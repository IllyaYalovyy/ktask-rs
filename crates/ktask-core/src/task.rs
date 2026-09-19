//! Task type and status enum.

use crate::TaskId;
use serde::{Deserialize, Serialize};

/// Task execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Task is pending execution.
    Pending,
    /// Task completed successfully.
    Done,
    /// Task failed.
    Failed,
    /// Task requires human input before proceeding.
    NeedsInput,
    /// Task is waiting at a human gate.
    HumanGate,
}

/// A task in the queue with its metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task identifier.
    pub id: TaskId,
    /// Current task status.
    pub status: TaskStatus,
    /// Full task body/description.
    pub body: String,
    /// What outcome is expected.
    pub outcome: String,
    /// When the task is considered done.
    pub done_when: String,
    /// How to verify the task is complete.
    pub verify: String,
    /// References and related resources.
    pub refs: String,
}

impl Task {
    /// Return the first line of the task body, truncated to 80 characters on a character boundary.
    ///
    /// Uses Unicode character boundaries to ensure multi-byte characters are never split.
    #[must_use]
    pub fn title(&self) -> &str {
        let first_line = self.body.lines().next().unwrap_or("");

        let char_count = first_line.chars().count();
        if char_count <= 80 {
            return first_line;
        }

        // Find the byte position of the 81st character (the first character after the 80-char limit)
        if let Some((byte_idx, _)) = first_line.char_indices().nth(80) {
            return &first_line[..byte_idx];
        }

        first_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_returns_first_line() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "First line\nSecond line\nThird line".to_string(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        assert_eq!(task.title(), "First line");
    }

    #[test]
    fn title_empty_body_returns_empty() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: String::new(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        assert_eq!(task.title(), "");
    }

    #[test]
    fn title_respects_80_char_boundary_ascii() {
        let long_line = "a".repeat(100);
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: long_line,
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        let title = task.title();
        assert_eq!(title.len(), 80);
        assert_eq!(title, "a".repeat(80));
    }

    #[test]
    fn title_shorter_than_80_chars_returns_full_line() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Short".to_string(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        assert_eq!(task.title(), "Short");
    }

    #[test]
    fn title_truncation_never_splits_multibyte_character() {
        // Create a task with a body that has many 4-byte emoji characters
        // Each emoji is 4 bytes; 80 characters would need the 81st to start
        let body = "🎯".repeat(100);
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body,
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        let title = task.title();

        // Verify the title is valid UTF-8 with no split characters
        let char_vec: Vec<char> = title.chars().collect();
        assert_eq!(char_vec.len(), 80, "Should have exactly 80 characters");
        assert!(char_vec.iter().all(|&c| c == '🎯'), "All should be emoji");

        // Verify it's on a character boundary
        assert!(title.is_char_boundary(0));
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn title_with_mixed_width_chars_never_splits() {
        // Mix of ASCII (1 byte) and emoji (4 bytes)
        let body = format!("{}🎯{}🎯{}", "a".repeat(40), "b".repeat(20), "c".repeat(15));
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body,
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        let title = task.title();

        // Verify it's valid UTF-8 by collecting all characters
        let chars: Vec<char> = title.chars().collect();
        assert!(chars.len() <= 80, "Should not exceed 80 characters");

        // Verify boundaries are never split
        assert!(title.is_char_boundary(0));
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn title_multibyte_at_boundary() {
        // Construct a string where the 80th character is multi-byte
        // 79 ASCII chars + 1 emoji
        let body = format!("{}🎯{}", "a".repeat(79), "x".repeat(50));
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body,
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        let title = task.title();

        // Should include the emoji since it's the 80th character
        assert!(
            title.contains('🎯'),
            "Should include the emoji at position 80"
        );
        let chars: Vec<char> = title.chars().collect();
        assert_eq!(chars.len(), 80, "Should be exactly 80 characters");

        // Verify no split
        assert!(title.is_char_boundary(0));
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn title_with_newlines_takes_first_only() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "First line\nSecond line that is very long".to_string(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };
        let title = task.title();
        assert_eq!(title, "First line");
        assert!(!title.contains('\n'));
    }

    #[test]
    fn task_status_serialization() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::Pending).unwrap(),
            "\"Pending\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Done).unwrap(),
            "\"Done\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Failed).unwrap(),
            "\"Failed\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::NeedsInput).unwrap(),
            "\"NeedsInput\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::HumanGate).unwrap(),
            "\"HumanGate\""
        );
    }
}

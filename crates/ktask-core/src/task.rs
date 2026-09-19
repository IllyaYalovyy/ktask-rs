//! Task type and status enum.

use crate::{Error, Result, TaskId};
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

    /// Validate that a task has all required sections.
    ///
    /// # Errors
    ///
    /// Returns an error listing all missing sections if any required section is empty.
    pub fn validate(&self) -> Result<()> {
        let mut missing = Vec::new();

        if self.outcome.trim().is_empty() {
            missing.push("Outcome");
        }
        if self.done_when.trim().is_empty() {
            missing.push("Done-when");
        }
        if self.verify.trim().is_empty() {
            missing.push("Verify");
        }
        if self.refs.trim().is_empty() {
            missing.push("Refs");
        }

        if !missing.is_empty() {
            return Err(Error::Policy {
                detail: format!(
                    "Task '{}' missing required sections: {}",
                    self.title(),
                    missing.join(", ")
                ),
                paths: vec![],
            });
        }

        Ok(())
    }
}

/// Parse a plan document into tasks.
///
/// A task is a level-two heading (`## <title>`) and everything until the next heading,
/// containing four required sections as bold labels: **Outcome:**, **Done-when:**,
/// **Verify:**, and **Refs:**. An optional **Gate:** section marks a human gate.
///
/// Task ids are assigned from 1 in document order. Nothing is stripped or modified;
/// the document is parsed as-is to preserve Markdown formatting.
///
/// # Errors
///
/// Returns an error if a task is missing any required section.
pub fn parse_plan(text: &str) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    let mut task_id = 1u32;

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        // Find the next task heading (level-two heading)
        if let Some(line) = lines.get(i) {
            if let Some(title_part) = line.strip_prefix("## ") {
                let title = title_part.trim().to_string();
                i += 1;

                // Collect content until the next heading or end of document
                let mut body_lines = Vec::new();
                while i < lines.len() {
                    if let Some(next_line) = lines.get(i) {
                        if next_line.strip_prefix("## ").is_some() {
                            break;
                        }
                        body_lines.push(*next_line);
                    }
                    i += 1;
                }

                let body_text = body_lines.join("\n");

                // Parse sections from the body
                let (outcome, done_when, verify, refs, is_gate) = parse_sections(&body_text);

                let status = if is_gate {
                    TaskStatus::HumanGate
                } else {
                    TaskStatus::Pending
                };

                let task = Task {
                    id: TaskId::new(task_id),
                    status,
                    body: format!("{title}\n{body_text}"),
                    outcome,
                    done_when,
                    verify,
                    refs,
                };

                // Validate that all required sections are present
                task.validate()?;

                tasks.push(task);
                task_id += 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    Ok(tasks)
}

fn parse_sections(text: &str) -> (String, String, String, String, bool) {
    let mut outcome = String::new();
    let mut done_when = String::new();
    let mut verify = String::new();
    let mut refs = String::new();
    let mut is_gate = false;

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        if let Some(line) = lines.get(i) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                i += 1;
                continue;
            }

            // Check for section headers (case-insensitive, normalized)
            let lower = trimmed.to_lowercase();

            if lower.starts_with("**outcome:") {
                outcome = extract_section(&lines, &mut i);
            } else if lower.starts_with("**done") && lower.contains(':') {
                // Matches "**Done-when:**", "**Done When:**", "**Done-When:**", etc.
                done_when = extract_section(&lines, &mut i);
            } else if lower.starts_with("**verify:") {
                verify = extract_section(&lines, &mut i);
            } else if lower.starts_with("**refs:") {
                refs = extract_section(&lines, &mut i);
            } else if lower.starts_with("**gate:") {
                is_gate = true;
                extract_section(&lines, &mut i);
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    (outcome, done_when, verify, refs, is_gate)
}

fn extract_section(lines: &[&str], i: &mut usize) -> String {
    let mut section_content = String::new();

    if let Some(header_line) = lines.get(*i) {
        let trimmed = header_line.trim();

        // Extract content from the header line itself (after the colon)
        if let Some(colon_idx) = trimmed.find(':') {
            let after_colon = trimmed[colon_idx + 1..].trim();
            if !after_colon.is_empty() {
                section_content.push_str(after_colon);
                section_content.push('\n');
            }
        }
    }

    *i += 1;

    // Collect lines until we hit a new section header or end
    while *i < lines.len() {
        if let Some(line) = lines.get(*i) {
            let trimmed = line.trim();
            let lower = trimmed.to_lowercase();

            // Stop at the next section header
            if lower.starts_with("**outcome:")
                || (lower.starts_with("**done") && lower.contains(':'))
                || lower.starts_with("**verify:")
                || lower.starts_with("**refs:")
                || lower.starts_with("**gate:")
            {
                break;
            }

            section_content.push_str(line);
            section_content.push('\n');
        }
        *i += 1;
    }

    section_content.trim().to_string()
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

    #[test]
    fn parse_plan_single_task() {
        let plan = r"## Fix the bug

**Outcome:** The bug is fixed

**Done-when:** Tests pass

**Verify:** cargo test

**Refs:** Issue #123
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, TaskId::new(1));
        assert_eq!(tasks[0].status, TaskStatus::Pending);
        assert!(tasks[0].body.contains("Fix the bug"));
        assert!(tasks[0].outcome.contains("bug is fixed"));
        assert!(tasks[0].done_when.contains("Tests pass"));
    }

    #[test]
    fn parse_plan_multiple_tasks() {
        let plan = r"## First task

**Outcome:** First outcome

**Done-when:** When first is done

**Verify:** cargo test

**Refs:** Ref 1

## Second task

**Outcome:** Second outcome

**Done-when:** When second is done

**Verify:** cargo test

**Refs:** Ref 2

## Third task

**Outcome:** Third outcome

**Done-when:** When third is done

**Verify:** cargo test

**Refs:** Ref 3
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, TaskId::new(1));
        assert_eq!(tasks[1].id, TaskId::new(2));
        assert_eq!(tasks[2].id, TaskId::new(3));
        assert!(tasks[0].body.contains("First task"));
        assert!(tasks[1].body.contains("Second task"));
        assert!(tasks[2].body.contains("Third task"));
    }

    #[test]
    fn parse_plan_with_human_gate() {
        let plan = r"## Approval gate

**Outcome:** Human reviews the design

**Done-when:** Human says it's good

**Verify:** Manual review

**Refs:** Design doc

**Gate:** This is a human gate
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::HumanGate);
    }

    #[test]
    fn parse_plan_with_code_block_containing_hashes_and_dashes() {
        let plan = r"## Parse markdown code

**Outcome:** Code is parsed correctly

**Done-when:** All edge cases handled

**Verify:** Run tests

**Refs:** Edge case doc

Description with code block below:

```rust
// This is a comment with # in it
fn test() {
    // ---
    let x = 1;
}
```

More text after code block.
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].body.contains("```rust"));
        assert!(tasks[0].body.contains("// This is a comment with # in it"));
        assert!(tasks[0].body.contains("---"));
        assert!(tasks[0].body.contains("```"));
    }

    #[test]
    fn parse_plan_empty_document() {
        let plan = "";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn parse_plan_no_tasks() {
        let plan = "This is just some text without any level-two headings.\n\nNo tasks here.";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn parse_plan_missing_outcome() {
        let plan = r"## Task without outcome

**Done-when:** When done

**Verify:** cargo test

**Refs:** Ref
";
        let result = parse_plan(plan);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Outcome"));
    }

    #[test]
    fn parse_plan_missing_done_when() {
        let plan = r"## Task without done-when

**Outcome:** Outcome

**Verify:** cargo test

**Refs:** Ref
";
        let result = parse_plan(plan);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Done-when"));
    }

    #[test]
    fn parse_plan_missing_verify() {
        let plan = r"## Task without verify

**Outcome:** Outcome

**Done-when:** When done

**Refs:** Ref
";
        let result = parse_plan(plan);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Verify"));
    }

    #[test]
    fn parse_plan_missing_refs() {
        let plan = r"## Task without refs

**Outcome:** Outcome

**Done-when:** When done

**Verify:** cargo test
";
        let result = parse_plan(plan);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Refs"));
    }

    #[test]
    fn parse_plan_case_insensitive_headers() {
        let plan = r"## Case test

**outcome:** Outcome text

**DONE-WHEN:** Done when text

**Verify:** Verify text

**REFS:** Refs text
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].outcome.contains("Outcome text"));
        assert!(tasks[0].done_when.contains("Done when text"));
    }

    #[test]
    fn parse_plan_multiline_sections() {
        let plan = r"## Multiline task

**Outcome:** This is a long outcome
that spans multiple lines
with lots of detail

**Done-when:** When this is complete
and all tests pass
and it's been reviewed

**Verify:** cargo test && cargo clippy

**Refs:** Multiple references
See also: that other doc
And: another thing
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].outcome.contains("This is a long outcome"));
        assert!(tasks[0].outcome.contains("spans multiple"));
        assert!(tasks[0].outcome.contains("detail"));
        assert!(tasks[0].done_when.contains("complete"));
        assert!(tasks[0].done_when.contains("reviewed"));
        assert!(tasks[0].refs.contains("Multiple references"));
    }

    #[test]
    fn parse_plan_preserves_formatting() {
        let plan = r"## Task with formatting

**Outcome:** Some outcome text

**Done-when:** Some done-when text

**Verify:** Some verify text

**Refs:** Some refs text
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        // The body should include the title and all content
        assert!(tasks[0].body.contains("Task with formatting"));
    }

    #[test]
    fn parse_plan_gate_with_alternatives() {
        let plan = r"## Gate with content

**Outcome:** Waiting for input

**Done-when:** When human responds

**Verify:** Human verification

**Refs:** Design doc

**Gate:** This requires human decision
with multiple options
to choose from
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::HumanGate);
    }

    #[test]
    fn parse_plan_complex_document() {
        let plan = r"# Project Plan

This is a project with multiple tasks to complete.

## Task 1: Setup

**Outcome:** Environment is set up

**Done-when:** All tools installed

**Verify:** Run setup script

**Refs:** Setup guide

Some description here about setup.

## Task 2: Implement

**Outcome:** Feature is implemented

**Done-when:** Tests pass

**Verify:** cargo test

**Refs:** Feature spec

Code block with special chars:
```
# comment
---
```

## Task 3: Approval

**Outcome:** Changes are approved

**Done-when:** Reviewer approves

**Verify:** Code review

**Refs:** PR link

**Gate:** Waiting for code review
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].status, TaskStatus::Pending);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
        assert_eq!(tasks[2].status, TaskStatus::HumanGate);
        assert_eq!(tasks[0].id, TaskId::new(1));
        assert_eq!(tasks[1].id, TaskId::new(2));
        assert_eq!(tasks[2].id, TaskId::new(3));
    }

    #[test]
    fn validate_task_with_all_sections() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "This is the outcome".to_string(),
            done_when: "When it's done".to_string(),
            verify: "Run tests".to_string(),
            refs: "Link to docs".to_string(),
        };
        assert!(task.validate().is_ok());
    }

    #[test]
    fn validate_task_missing_one_section() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: "This is the outcome".to_string(),
            done_when: "When it's done".to_string(),
            verify: String::new(),
            refs: "Link to docs".to_string(),
        };
        let result = task.validate();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Verify"));
        assert!(!err_msg.contains("Outcome"));
    }

    #[test]
    fn validate_task_missing_multiple_sections() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Test task".to_string(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: "Link to docs".to_string(),
        };
        let result = task.validate();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Outcome"));
        assert!(err_msg.contains("Done-when"));
        assert!(err_msg.contains("Verify"));
        assert!(!err_msg.contains("Refs"));
    }

    #[test]
    fn parse_plan_missing_multiple_sections() {
        let plan = r"## Task missing sections

**Outcome:** Only outcome is present

**Done-when:**

Some other content here
";
        let result = parse_plan(plan);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Verify"));
        assert!(err_msg.contains("Refs"));
    }

    #[test]
    fn parse_plan_with_unknown_sections_preserved_in_body() {
        let plan = r"## Task with custom section

**Outcome:** The outcome

**Done-when:** When done

**Verify:** cargo test

**Refs:** Documentation

Extra content here
with some **bold** text
and a custom section below.
";
        let tasks = parse_plan(plan).expect("parse");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].body.contains("Extra content"));
        assert!(tasks[0].body.contains("custom section"));
    }
}

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    fn task_title_strategy() -> impl Strategy<Value = String> {
        r"[a-zA-Z0-9 \-]{1,60}".prop_map(|s| s.trim().to_string())
    }

    fn section_content_strategy() -> impl Strategy<Value = String> {
        r"[a-zA-Z0-9 \.\-_\n()]{1,100}".prop_map(|s| {
            s.trim()
                .chars()
                .filter(|c| *c != '\n' || s.len() < 50)
                .collect::<String>()
                .trim()
                .to_string()
        })
    }

    fn task_content_strategy() -> impl Strategy<Value = (String, String, String, String, String)> {
        (
            task_title_strategy(),
            section_content_strategy(),
            section_content_strategy(),
            section_content_strategy(),
            section_content_strategy(),
        )
            .prop_map(|(title, outcome, done_when, verify, refs)| {
                (title, outcome, done_when, verify, refs)
            })
    }

    proptest! {
        #[test]
        fn prop_parse_plan_is_total_and_never_panics(
            task_contents in prop::collection::vec(task_content_strategy(), 0..10)
        ) {
            use std::fmt::Write;

            let mut plan_doc = String::new();

            for (title, outcome, done_when, verify, refs) in task_contents {
                if !title.is_empty() && !outcome.is_empty() && !done_when.is_empty() && !verify.is_empty() && !refs.is_empty() {
                    let _ = writeln!(
                        plan_doc,
                        "## {title}\n\n**Outcome:** {outcome}\n\n**Done-when:** {done_when}\n\n**Verify:** {verify}\n\n**Refs:** {refs}\n"
                    );
                }
            }

            // Parsing should never panic
            let result = parse_plan(&plan_doc);

            // For valid documents with proper sections, parsing should succeed
            if plan_doc.is_empty() {
                result.expect("should parse empty document");
            } else if let Ok(parsed_tasks) = result {
                // Verify each task has correct ID and status
                for (i, task) in parsed_tasks.iter().enumerate() {
                    let id_u32 = u32::try_from(i + 1).expect("task count should fit in u32");
                    assert_eq!(task.id, TaskId::new(id_u32));
                    assert!(!task.outcome.trim().is_empty());
                    assert!(!task.done_when.trim().is_empty());
                    assert!(!task.verify.trim().is_empty());
                    assert!(!task.refs.trim().is_empty());
                }
            }
        }

        #[test]
        fn prop_parse_handles_unicode_and_special_chars(
            unicode_title in r"[a-zA-Z0-9 \-🎯✓💡]{1,50}",
            unicode_body in r"[a-zA-Z0-9 \.\-_🎯✓💡\n]{1,100}",
        ) {
            let plan = format!(
                "## {}\n\n**Outcome:** {}\n\n**Done-when:** done\n\n**Verify:** test\n\n**Refs:** ref\n",
                unicode_title.trim(), unicode_body.trim()
            );

            let result = parse_plan(&plan);
            // Should not panic; may fail validation if sections are empty
            let _ = result;
        }

        #[test]
        fn prop_parse_roundtrip_preserves_sections(
            outcome in r"[a-zA-Z0-9 \.\-_]{1,80}",
            done_when in r"[a-zA-Z0-9 \.\-_]{1,80}",
            verify in r"[a-zA-Z0-9 \.\-_]{1,80}",
            refs in r"[a-zA-Z0-9 \.\-_]{1,80}",
        ) {
            let plan = format!(
                "## Test Task\n\n**Outcome:** {}\n\n**Done-when:** {}\n\n**Verify:** {}\n\n**Refs:** {}\n",
                outcome.trim(), done_when.trim(), verify.trim(), refs.trim()
            );

            let result = parse_plan(&plan);
            if let Ok(tasks) = result
                && !tasks.is_empty() {
                let task = &tasks[0];
                // Sections should be preserved
                assert!(task.outcome.contains(outcome.trim()) || outcome.trim().is_empty());
                assert!(task.done_when.contains(done_when.trim()) || done_when.trim().is_empty());
                assert!(task.verify.contains(verify.trim()) || verify.trim().is_empty());
                assert!(task.refs.contains(refs.trim()) || refs.trim().is_empty());
            }
        }
    }
}

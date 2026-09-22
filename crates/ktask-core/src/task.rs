//! The in-memory task model: `Task` and `TaskStatus`.
//!
//! A `Task` is the unit of work a run supervises. Its fields mirror the
//! Markdown task format described in `.ktask/README.md`: `outcome`,
//! `done_when`, `verify` and `refs` come directly from the `**Outcome:**`,
//! `**Done-when:**`, `**Verify:**` and `**Refs:**` sections of a task, while
//! `body` holds the task's full Markdown text.

use crate::{Error, Result, TaskId};

/// The maximum number of characters kept in a task's [`Task::title`].
const TITLE_MAX_CHARS: usize = 80;

/// The bold section labels every task must carry, in the order
/// [`validate`] reports them.
const REQUIRED_SECTIONS: [&str; 4] = ["Outcome", "Done-when", "Verify", "Refs"];

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

/// Checks that `task` carries every required section: `Outcome`,
/// `Done-when`, `Verify` and `Refs`.
///
/// # Errors
///
/// Returns [`Error::Policy`] naming every missing section (not just the
/// first) if one or more of the four required fields is empty.
pub fn validate(task: &Task) -> Result<()> {
    let fields = [&task.outcome, &task.done_when, &task.verify, &task.refs];
    let missing: Vec<&str> = REQUIRED_SECTIONS
        .into_iter()
        .zip(fields)
        .filter(|(_, content)| content.trim().is_empty())
        .map(|(label, _)| label)
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::Policy {
            detail: format!(
                "task is missing required section(s): {}",
                missing.join(", ")
            ),
            paths: Vec::new(),
        })
    }
}

/// Splits a plan document into its tasks.
///
/// A task is a level-two heading (`## <title>`) and every line up to, but
/// excluding, the next one; ids are assigned from 1 in document order. A
/// `**Gate:**` line inside a task marks it a human gate rather than
/// ordinary work.
///
/// Nothing in a task's content is inspected or rewritten: a fenced code
/// block (opened with three or more backticks or tildes) is copied through
/// untouched, so a `#`, `##` or `---` line inside one never opens a heading
/// or a gate section. Content before the first heading is not part of any
/// task and is discarded. Extracting the four required sections into their
/// fields, and rejecting a task that is missing one, is not this function's
/// job.
///
/// # Errors
///
/// Currently infallible: every document, including one with no headings at
/// all, parses to some (possibly empty) list of tasks. The `Result` leaves
/// room for a later structural check without changing callers.
pub fn parse_plan(text: &str) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    let mut fence: Option<(char, usize)> = None;
    let mut next_id: u32 = 1;

    for line in text.lines() {
        if let Some((fence_char, fence_len)) = fence {
            if let Some((close_char, close_len)) = fence_delimiter(line)
                && close_char == fence_char
                && close_len >= fence_len
            {
                fence = None;
            }
        } else if let Some(delimiter) = fence_delimiter(line) {
            fence = Some(delimiter);
        } else if let Some(title) = line.strip_prefix("## ") {
            if let Some(lines) = current.take() {
                tasks.push(build_task(next_id, &lines));
                next_id += 1;
            }
            current = Some(vec![title]);
            continue;
        }

        if let Some(lines) = current.as_mut() {
            lines.push(line);
        }
    }

    if let Some(lines) = current.take() {
        tasks.push(build_task(next_id, &lines));
    }

    Ok(tasks)
}

/// Returns the fence character and run length if `line` opens or closes a
/// fenced code block.
fn fence_delimiter(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    (len >= 3).then_some((ch, len))
}

/// Builds a `Task` from a heading block: `lines[0]` is the title (already
/// stripped of `## `), and the rest is the task's body content, in order.
fn build_task(id: u32, lines: &[&str]) -> Task {
    let status = if lines
        .iter()
        .any(|line| line.trim_start().starts_with("**Gate:**"))
    {
        TaskStatus::HumanGate
    } else {
        TaskStatus::Pending
    };
    let sections = extract_sections(lines.get(1..).unwrap_or_default());
    let section = |label: &str| {
        sections
            .iter()
            .find(|(found, _)| found == label)
            .map_or_else(String::new, |(_, content)| content.clone())
    };
    Task {
        id: TaskId::new(id),
        status,
        body: lines.join("\n"),
        outcome: section("Outcome"),
        done_when: section("Done-when"),
        verify: section("Verify"),
        refs: section("Refs"),
    }
}

/// Parses every `**Label:** ...` section out of a task's body lines, in
/// document order.
///
/// A section starts at a bold label and runs until the next bold label or
/// the end of the block, so both the four required sections and any extra,
/// unrecognized ones are captured; [`build_task`] picks the four it needs by
/// name and leaves the rest unused (`body` already holds the untouched
/// original text, so nothing is lost).
fn extract_sections(lines: &[&str]) -> Vec<(String, String)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();

    for line in lines {
        if let Some((label, rest)) = bold_label(line) {
            sections.push((label.to_string(), vec![rest.to_string()]));
        } else if let Some((_, content)) = sections.last_mut() {
            content.push((*line).to_string());
        }
    }

    sections
        .into_iter()
        .map(|(label, content)| (label, content.join("\n").trim().to_string()))
        .collect()
}

/// Recognizes a line that opens with a bold section label, such as
/// `**Outcome:** the rest of the line`.
///
/// Returns the label and whatever follows it on the same line. The label
/// must sit at the very start of the (trimmed) line, so a bold, colon-ended
/// phrase in the middle of a sentence is not mistaken for a section.
fn bold_label(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let after_open = trimmed.strip_prefix("**")?;
    let close = after_open.find(":**")?;
    let label = &after_open[..close];
    (!label.is_empty()).then(|| (label, after_open[close + 3..].trim_start()))
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

    #[test]
    fn a_plan_with_one_task_parses_it() {
        let plan = "\
## Do the thing

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, TaskId::new(1));
        assert_eq!(tasks[0].title(), "Do the thing");
        assert_eq!(tasks[0].status, TaskStatus::Pending);
        assert!(tasks[0].body.contains("**Outcome:** it happens."));
    }

    #[test]
    fn a_plan_with_many_tasks_assigns_ids_from_one_in_document_order() {
        let plan = "\
## First

body one

## Second

body two

## Third

body three
";
        let tasks = parse_plan(plan).unwrap();
        let summary: Vec<(u32, &str)> = tasks.iter().map(|t| (t.id.get(), t.title())).collect();
        assert_eq!(summary, vec![(1, "First"), (2, "Second"), (3, "Third")]);
    }

    #[test]
    fn a_gate_section_marks_the_task_a_human_gate() {
        let plan = "\
## Ship it

**Gate:** a human must approve the release notes.
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::HumanGate);
    }

    #[test]
    fn a_task_without_a_gate_section_is_pending() {
        let plan = "\
## Just do it

**Outcome:** done.
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Pending);
    }

    #[test]
    fn a_fenced_code_block_is_passed_through_untouched() {
        let plan = "\
## Task with a fence

**Outcome:** the fence below survives parsing intact.

```text
# this is a shell comment, not a heading
## and this is not a new task either
---
still inside the fence
```

**Done-when:** the fence above appears verbatim in body.

## Next task

after the fence
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title(), "Task with a fence");
        assert!(tasks[0].body.contains("```text"));
        assert!(
            tasks[0]
                .body
                .contains("# this is a shell comment, not a heading")
        );
        assert!(
            tasks[0]
                .body
                .contains("## and this is not a new task either")
        );
        assert!(tasks[0].body.contains("---\nstill inside the fence"));
        assert_eq!(tasks[1].title(), "Next task");
        assert!(tasks[1].body.contains("after the fence"));
    }

    #[test]
    fn a_document_with_no_tasks_is_an_empty_list() {
        let plan = "\
# Just a title

Some prose with no level-two heading.
";
        let tasks = parse_plan(plan).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn an_empty_document_is_an_empty_list() {
        assert!(parse_plan("").unwrap().is_empty());
    }

    #[test]
    fn parsing_extracts_the_four_required_sections_into_their_fields() {
        let plan = "\
## Do the thing

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(tasks[0].outcome, "it happens.");
        assert_eq!(tasks[0].done_when, "it happened.");
        assert_eq!(tasks[0].verify, "`true`");
        assert_eq!(tasks[0].refs, "none");
    }

    #[test]
    fn a_section_runs_until_the_next_bold_label_including_extra_lines() {
        let plan = "\
## Multi-line sections

**Outcome:** the first line
and a second line of outcome.

**Done-when:** done.
**Verify:** `cargo test`
**Refs:** VISION.md
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(
            tasks[0].outcome,
            "the first line\nand a second line of outcome."
        );
        assert_eq!(tasks[0].done_when, "done.");
        assert_eq!(tasks[0].verify, "`cargo test`");
        assert_eq!(tasks[0].refs, "VISION.md");
    }

    #[test]
    fn an_unknown_section_is_preserved_in_body_and_does_not_shadow_required_ones() {
        let plan = "\
## Task with an extra section

**Outcome:** it happens.

**Files:** src/lib.rs

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
";
        let tasks = parse_plan(plan).unwrap();
        assert_eq!(tasks[0].outcome, "it happens.");
        assert_eq!(tasks[0].done_when, "it happened.");
        assert!(tasks[0].body.contains("**Files:** src/lib.rs"));
        assert!(validate(&tasks[0]).is_ok());
    }

    #[test]
    fn validate_accepts_a_task_with_all_four_required_sections() {
        let plan = "\
## Complete task

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
";
        let tasks = parse_plan(plan).unwrap();
        assert!(validate(&tasks[0]).is_ok());
    }

    #[test]
    fn validate_names_every_missing_section_not_just_the_first() {
        let plan = "\
## Incomplete task

**Outcome:** it happens.

**Verify:** `true`
";
        let tasks = parse_plan(plan).unwrap();
        let err = validate(&tasks[0]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Done-when"));
        assert!(message.contains("Refs"));
        assert!(!message.contains("Outcome"));
        assert!(!message.contains("Verify"));
    }

    #[test]
    fn validate_rejects_a_task_missing_every_section() {
        let plan = "\
## Bare task

Nothing but prose here.
";
        let tasks = parse_plan(plan).unwrap();
        let err = validate(&tasks[0]).unwrap_err();
        let message = err.to_string();
        for label in ["Outcome", "Done-when", "Verify", "Refs"] {
            assert!(message.contains(label), "expected {label} in {message}");
        }
    }
}

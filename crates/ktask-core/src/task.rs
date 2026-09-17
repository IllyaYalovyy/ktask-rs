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
//! [`parse_plan`] is where a plan document becomes tasks. It reads the file as
//! the document it is: only a level-two heading divides a task, nothing a line
//! may begin with is reserved, and a block is kept byte for byte — a supervisor
//! imports a plan and never edits one in place (VISION.md §4, ADR-0007).
//!
//! The state machine that drives a task through preflight, attempts and gates is
//! `TaskState` in `state.rs`, a different and much richer type. This is the
//! queue entry; that is the run.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
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
    /// The `Gate:` section — what a person is being asked to decide, present
    /// only on a task that is a human gate.
    ///
    /// A gate is marked by this section rather than by [`TaskStatus::HumanGate`]
    /// because status is what the supervisor has concluded, and a task that has
    /// just been imported has not been run, paused or concluded on yet.
    pub gate: Option<String>,
}

/// How many characters [`Task::title`] keeps before it cuts the line.
///
/// A count of characters rather than bytes, so the cut is the same for a task
/// written in any script: 80 Latin letters and 80 ideographs each fill the same
/// slot.
const TITLE_MAX_CHARS: usize = 80;

/// The sections every task must carry, named as the document writes them.
const REQUIRED_SECTIONS: [&str; 4] = ["Outcome", "Done-when", "Verify", "Refs"];

/// The label whose section makes a task a human gate.
const GATE_SECTION: &str = "Gate";

/// The most indentation a heading may have and still be a heading.
///
/// Four spaces is an indented code block, so a `## ` written that deep is a
/// line of somebody's example rather than a task to queue.
const MAX_HEADING_INDENT: usize = 3;

/// The characters that separate a section from its label and from the next
/// section, and nothing else: text inside a section is never trimmed.
const SECTION_PADDING: [char; 4] = [' ', '\t', '\r', '\n'];

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

/// One line of a plan document, and whether a code fence hides its markup.
#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    /// The line exactly as written, break included.
    raw: &'a str,
    /// Whether a fenced code block contains the line, so its `#` and its bold
    /// labels are content rather than markup.
    inside_fence: bool,
    /// The 1-based number of the line, which is where a reader goes.
    number: usize,
}

/// A task's lines: the heading that opens it and everything until the next one.
struct Block<'a> {
    /// The heading as written, minus its indentation and break, which names the
    /// block in the message a human acts on.
    heading: &'a str,
    /// The line the heading sits on, counted from one.
    starts_at: usize,
    /// The heading line and the lines below it, exactly as authored.
    lines: Vec<Line<'a>>,
}

/// Parse a plan document into the tasks it holds, numbered from one in the
/// order they are written.
///
/// A task is a level-two heading and everything until the next one, and the
/// document stays ordinary Markdown on the way in: a line may begin with `#`,
/// a horizontal rule is a horizontal rule, and a fenced code block is passed
/// through untouched — inside a fence a heading opens no task and a bold label
/// opens no section, because a supervisor reads this file and never edits it
/// (VISION.md §4). Nothing is stripped: [`Task::body`] is the block byte for
/// byte, and no status marker is written back, because status lives in the
/// database.
///
/// A `**Gate:**` section marks the task a human gate and is kept as
/// [`Task::gate`]; the four required sections are kept in their own fields, and
/// any other bold label is kept in the body alone.
///
/// # Errors
///
/// [`Error::NotFound`] when a block lacks a required section, naming every
/// section it lacks and the line its heading sits on. A malformed task never
/// enters the queue (VISION.md §4), so this is also what `add --file` rejects.
pub fn parse_plan(text: &str) -> Result<Vec<Task>> {
    split_blocks(&scan_lines(text))
        .iter()
        .enumerate()
        .map(|(index, block)| build_task(task_id(index)?, block))
        .collect()
}

/// The id of the task at `index` in document order, counted from one.
fn task_id(index: usize) -> Result<TaskId> {
    let position = index.saturating_add(1);
    let Ok(number) = u32::try_from(position) else {
        return Err(Error::Corrupt {
            detail: format!(
                "task {position} is past the {largest} a task id can hold",
                largest = u32::MAX
            ),
            seq: None,
        });
    };
    Ok(TaskId::new(number))
}

/// The lines of a document in order, each marked with whether a fence hides it.
///
/// A fence opens on a run of at least three backticks or tildes and closes on
/// the next run of the same character that is at least as long and carries
/// nothing else. Both those lines are part of the fence: neither is a heading
/// or a label.
fn scan_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (index, raw) in text.split_inclusive('\n').enumerate() {
        let inside_fence = advance_fence(&mut fence, unfold(raw).1);
        lines.push(Line {
            raw,
            inside_fence,
            number: index + 1,
        });
    }
    lines
}

/// Whether a line sits inside a fence, advancing `fence` across the line.
///
/// The line that opens a fence and the line that closes it are inside it: a
/// fence is content, so neither of its own delimiters is markup either.
fn advance_fence(fence: &mut Option<(char, usize)>, text: &str) -> bool {
    if let Some((marker, run)) = *fence {
        if closes_fence(text, marker, run) {
            *fence = None;
        }
        return true;
    }
    let opened = opens_fence(text);
    *fence = opened;
    opened.is_some()
}

/// The fence a line opens: the repeated character and how many of them.
///
/// The characters after the run are the language tag, which is the fence's
/// business and not this parser's.
fn opens_fence(text: &str) -> Option<(char, usize)> {
    let marker = text.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = text.chars().take_while(|c| *c == marker).count();
    (run >= 3).then_some((marker, run))
}

/// Whether a line ends the fence `marker` opened `run` characters deep.
fn closes_fence(text: &str, marker: char, run: usize) -> bool {
    let count = text.chars().take_while(|c| *c == marker).count();
    count >= run && text.chars().all(|c| c == marker)
}

/// The task blocks of a document, in order.
///
/// Everything before the first heading is a preamble: it is a document a
/// person reads, and a preamble is not a task to queue.
fn split_blocks<'a>(lines: &[Line<'a>]) -> Vec<Block<'a>> {
    let mut blocks: Vec<Block<'a>> = Vec::new();
    for line in lines {
        if opens_task(line) {
            blocks.push(Block {
                heading: unfold(line.raw).1,
                starts_at: line.number,
                lines: Vec::new(),
            });
        }
        if let Some(block) = blocks.last_mut() {
            block.lines.push(*line);
        }
    }
    blocks
}

/// Whether the line is the level-two heading that opens a task.
///
/// Exactly two hashes, at most [`MAX_HEADING_INDENT`] spaces or tabs in front
/// of them, and then a space or the end of the line. `### Deeper` is a
/// subheading inside the task above it, `# Shallower` is a heading of the
/// document, and `##NoSpace` is prose: nothing here is reserved.
fn opens_task(line: &Line<'_>) -> bool {
    if line.inside_fence {
        return false;
    }
    let (indent, text) = unfold(line.raw);
    indent <= MAX_HEADING_INDENT
        && matches!(text.strip_prefix("##"), Some(rest) if rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// A line without the whitespace around it, and how far it was indented.
fn unfold(raw: &str) -> (usize, &str) {
    let written = raw.trim_end_matches(['\r', '\n']);
    let text = written.trim_start_matches([' ', '\t']);
    (written.len() - text.len(), text)
}

/// The text of every labelled section in a block, keyed by the label without
/// its colon. A label written twice keeps the text written under it first.
fn labelled_sections<'a>(lines: &[Line<'a>]) -> BTreeMap<&'a str, String> {
    let mut sections = BTreeMap::new();
    let mut label: Option<&'a str> = None;
    let mut text = String::new();
    for line in lines {
        let written = unfold(line.raw).1;
        let started = if line.inside_fence {
            None
        } else {
            bold_label(written)
        };
        match started {
            Some((name, rest)) => {
                store(&mut sections, label.take(), &mut text);
                label = Some(name);
                text.push_str(rest);
            }
            None if label.is_some() => text.push_str(written),
            None => {}
        }
        text.push('\n');
    }
    store(&mut sections, label, &mut text);
    sections
}

/// The bold label a line opens with, and the text that follows it on the line.
fn bold_label(text: &str) -> Option<(&str, &str)> {
    let (label, rest) = text.strip_prefix("**")?.split_once("**")?;
    let name = label.strip_suffix(':')?;
    (!name.is_empty()).then_some((name, rest.trim_start_matches([' ', '\t'])))
}

/// Keeps the section that just ended under its label, holding the first text
/// written there, and empties the buffer for the section after it.
fn store<'a>(sections: &mut BTreeMap<&'a str, String>, label: Option<&'a str>, text: &mut String) {
    let written = std::mem::take(text);
    if let Some(label) = label {
        sections
            .entry(label)
            .or_insert_with(|| written.trim_matches(SECTION_PADDING).to_owned());
    }
}

/// The task a block describes, or the sections it is missing.
fn build_task(id: TaskId, block: &Block<'_>) -> Result<Task> {
    let sections = labelled_sections(&block.lines);
    let missing: Vec<&str> = REQUIRED_SECTIONS
        .into_iter()
        .filter(|name| !sections.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        return Err(Error::NotFound {
            what: missing_message(block, &missing),
        });
    }
    Ok(Task {
        id,
        status: TaskStatus::Pending,
        body: block_text(&block.lines),
        outcome: text_of(&sections, "Outcome"),
        done_when: text_of(&sections, "Done-when"),
        verify: text_of(&sections, "Verify"),
        refs: text_of(&sections, "Refs"),
        gate: sections.get(GATE_SECTION).cloned(),
    })
}

/// The block's text byte for byte: what a task keeps as its body is the
/// document it came from, with nothing stripped.
fn block_text(lines: &[Line<'_>]) -> String {
    lines.iter().map(|line| line.raw).collect()
}

/// The text of one section, empty when the block carries no such label.
fn text_of(sections: &BTreeMap<&str, String>, name: &str) -> String {
    sections.get(name).cloned().unwrap_or_default()
}

/// Why a block cannot be queued: every section it lacks, the heading that
/// opens it, and the line a reader should open the document at.
fn missing_message(block: &Block<'_>, missing: &[&str]) -> String {
    let named = missing
        .iter()
        .map(|name| format!("`{name}:`"))
        .collect::<Vec<_>>()
        .join(" and ");
    let word = if missing.len() == 1 {
        "section"
    } else {
        "sections"
    };
    format!(
        "the {named} {word} of `{}` (line {})",
        block.heading, block.starts_at
    )
}

#[cfg(test)]
mod tests {
    use super::{Task, TaskStatus, parse_plan, task_id};
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
            gate: None,
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

    /// A task block with the four required sections and nothing else, so a
    /// test can vary one part of a document without repeating all of it.
    fn task_block(title: &str) -> String {
        format!(
            "## {title}\n\n**Outcome:** what changes.\n\n**Done-when:** a test \
             asserts it.\n\n**Verify:** `cargo test`\n\n**Refs:** VISION.md\n"
        )
    }

    #[test]
    fn a_plan_with_one_task_parses_its_id_sections_and_body() {
        let document = "\
## T008 Plan document parser

**Outcome:** a plan document parses into tasks.

**Done-when:** a test asserts the parse.

**Verify:** `cargo nextest run -p ktask-core`

**Refs:** VISION.md section 4
";
        let tasks = parse_plan(document).expect("a well-formed plan parses");
        assert_eq!(tasks.len(), 1);
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(task.id, TaskId::new(1));
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.outcome, "a plan document parses into tasks.");
        assert_eq!(task.done_when, "a test asserts the parse.");
        assert_eq!(task.verify, "`cargo nextest run -p ktask-core`");
        assert_eq!(task.refs, "VISION.md section 4");
        assert_eq!(task.gate, None);
        assert_eq!(task.body, document, "the block is kept exactly as authored");
    }

    #[test]
    fn a_plan_with_many_tasks_numbers_them_from_one_in_document_order() {
        let document = "\
# ktask-rs implementation queue.

A preamble a reader sees and the parser never queues.

---

## T001 First

**Outcome:** one.
**Done-when:** asserted.
**Verify:** `cargo test`
**Refs:** none

## T002 Second

**Outcome:** two.
**Done-when:** asserted.
**Verify:** `cargo test`
**Refs:** none

## T003 Third

**Outcome:** three.
**Done-when:** asserted.
**Verify:** `cargo test`
**Refs:** none
";
        let tasks = parse_plan(document).expect("three well-formed tasks parse");
        let ids: Vec<TaskId> = tasks.iter().map(|task| task.id).collect();
        assert_eq!(ids, vec![TaskId::new(1), TaskId::new(2), TaskId::new(3)]);
        let outcomes: Vec<&str> = tasks.iter().map(|task| task.outcome.as_str()).collect();
        assert_eq!(outcomes, vec!["one.", "two.", "three."]);
        assert_eq!(tasks.len(), 3, "the preamble is prose, not a task");
        assert!(tasks[0].body.starts_with("## T001 First\n"));
        assert!(tasks[2].body.ends_with("**Refs:** none\n"));
        assert!(
            tasks[1].body.ends_with("\n\n"),
            "the blank line before a heading belongs to the block above it"
        );
    }

    #[test]
    fn a_document_with_no_tasks_parses_to_no_tasks_rather_than_an_error() {
        let document = "\
# ktask-rs implementation queue.

A line may begin with a hash without being a task:

# a level-one heading
#### and a level-four one

---

A horizontal rule is a horizontal rule.
";
        assert_eq!(
            parse_plan(document).expect("a plan with no tasks is not an error"),
            Vec::new()
        );
        assert_eq!(
            parse_plan("").expect("an empty document holds no tasks"),
            Vec::new()
        );
    }

    #[test]
    fn a_fenced_code_block_is_passed_through_untouched_and_never_opens_a_task() {
        let document = "\
## T008 Plan document parser

**Outcome:** a fenced block survives the parse intact.

**Done-when:** a test asserts the fence is byte for byte what was written.

**Verify:** `cargo nextest run -p ktask-core`

**Do:** the fence below is passed through untouched, and nothing inside it is markup:

```sh
# a comment that begins with a hash
## a level-two heading inside a fence is not a task
---
**Gate:** a fence cannot make a human gate
**Refs:** a fence cannot fill a section either
```

**Refs:** VISION.md section 4
";
        let tasks = parse_plan(document).expect("a fenced block does not break a task");
        assert_eq!(tasks.len(), 1, "a heading inside a fence opens nothing");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(task.body, document, "the fence is passed through untouched");
        assert!(task.body.contains("# a comment that begins with a hash"));
        assert!(
            task.body
                .contains("## a level-two heading inside a fence is not a task")
        );
        assert!(
            task.body.contains("---\n"),
            "a rule inside a fence is still a rule"
        );
        assert_eq!(
            task.refs, "VISION.md section 4",
            "a label inside a fence fills nothing"
        );
        assert_eq!(
            task.gate, None,
            "a `**Gate:**` inside a fence marks nothing"
        );
    }

    #[test]
    fn a_gate_section_marks_a_human_gate_and_keeps_what_it_asks() {
        let document = "\
## T050 Approve the migration

**Outcome:** a human reads the migration before any of it runs.

**Done-when:** the run stops here until someone acknowledges it.

**Gate:** approve the migration, or say what to change.

**Verify:** nothing runs; this task produces no commit.

**Refs:** VISION.md section 6

## T051 Carry on

**Outcome:** work continues past the gate.
**Done-when:** asserted.
**Verify:** `cargo test`
**Refs:** none
";
        let tasks = parse_plan(document).expect("a gate task parses like any other");
        assert_eq!(tasks.len(), 2);
        assert_eq!(
            tasks[0].gate.as_deref(),
            Some("approve the migration, or say what to change."),
            "the gate section says what the human is being asked to decide"
        );
        assert_eq!(tasks[1].gate, None, "the task below the gate is executable");
        assert_eq!(
            tasks[0].status,
            TaskStatus::Pending,
            "a gate is marked by its section, not by a status: nothing has run yet"
        );
        assert_eq!(
            tasks[0].refs, "VISION.md section 6",
            "the gate ends no section early"
        );
    }

    #[test]
    fn a_gate_section_with_no_text_still_marks_a_human_gate() {
        let document = "\
## T052 A gate that asks in prose

**Outcome:** a thing.
**Done-when:** asserted.
**Verify:** `cargo test`
**Refs:** none
**Gate:**
";
        let tasks = parse_plan(document).expect("an empty gate section is still a gate");
        assert_eq!(tasks.first().expect("one task").gate.as_deref(), Some(""));
    }

    #[test]
    fn a_task_missing_a_required_section_is_rejected_naming_every_missing_one() {
        let document = "\
# preamble

## T012 Notes on the format

**Outcome:** a document a person reads.

**Done-when:** a test asserts it.
";
        let error = parse_plan(document).expect_err("a task with no Verify cannot enter the queue");
        assert_eq!(
            error.to_string(),
            "not found: the `Verify:` and `Refs:` sections of `## T012 Notes on the format` \
             (line 3)"
        );
    }

    #[test]
    fn a_task_missing_one_section_is_reported_as_that_one_section_alone() {
        let document = "\
## T013 Almost complete

**Outcome:** a thing.
**Done-when:** a test asserts it.
**Verify:** `cargo test`
";
        let error = parse_plan(document).expect_err("a task with no Refs cannot enter the queue");
        assert_eq!(
            error.to_string(),
            "not found: the `Refs:` section of `## T013 Almost complete` (line 1)"
        );
    }

    #[test]
    fn a_section_runs_to_the_next_bold_label_and_an_unknown_label_ends_it() {
        let document = "\
## T014 Journal every transition

**Outcome:** transitions are persisted first.

That is the whole of the outcome's second paragraph.

**Files:** crates/ktask-core/src/journal.rs

**Done-when:** replay reproduces state.
**Verify:** `cargo nextest run -p ktask-core`
**Refs:** VISION.md section 6
";
        let tasks = parse_plan(document).expect("an unknown section is not an error");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.outcome,
            "transitions are persisted first.\n\nThat is the whole of the outcome's second \
             paragraph."
        );
        assert_eq!(task.done_when, "replay reproduces state.");
        assert!(
            task.body
                .contains("**Files:** crates/ktask-core/src/journal.rs"),
            "a section the model does not keep is still in the body"
        );
    }

    #[test]
    fn a_repeated_label_keeps_the_first_section_and_does_not_merge_the_two() {
        let document = "\
## T015 A duplicated label

**Outcome:** the first outcome.

**Outcome:** the second outcome.

**Done-when:** asserted.
**Verify:** `cargo test`
**Refs:** none
";
        let tasks = parse_plan(document).expect("a repeated label is not an error");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(task.outcome, "the first outcome.");
        assert!(task.body.contains("**Outcome:** the second outcome."));
    }

    #[test]
    fn only_a_level_two_heading_opens_a_task() {
        let document = format!(
            "{}\n### A step inside the task\n\n# not a task\n\n\
             ##NotAHeading because it has no space after the hashes\n",
            task_block("T020 Headings that do not count")
        );
        let tasks = parse_plan(&document).expect("only `## ` divides a task");
        assert_eq!(tasks.len(), 1);
        let body = &tasks.first().expect("one task").body;
        assert_eq!(body, &document);
        assert!(body.contains("### A step inside the task"));
        assert!(body.contains("# not a task"));
        assert!(body.contains("##NotAHeading because it has no space"));
    }

    #[test]
    fn a_level_two_heading_indented_past_three_spaces_is_prose() {
        let document = format!(
            "{}\n    ## not a task: four spaces indent a code block\n",
            task_block("T021 An indented heading")
        );
        let tasks = parse_plan(&document).expect("an indented heading is body text");
        assert_eq!(tasks.len(), 1);
        assert!(
            tasks
                .first()
                .expect("one task")
                .body
                .contains("    ## not a task")
        );
    }

    #[test]
    fn a_document_with_windows_line_endings_keeps_its_breaks_in_the_body() {
        let document = "## T022 Authored on Windows\r\n\r\n**Outcome:** keeps its breaks.\
                        \r\n**Done-when:** asserted.\r\n**Verify:** `cargo test`\r\n**Refs:** none\r\n";
        let tasks = parse_plan(document).expect("a CRLF plan is still a plan");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.body, document,
            "the break that was written is the break stored"
        );
        assert_eq!(task.outcome, "keeps its breaks.");
        assert_eq!(task.refs, "none");
    }

    #[test]
    fn ids_count_from_one_and_stop_at_the_largest_a_task_id_can_hold() {
        assert_eq!(
            task_id(0).expect("the first task is number one"),
            TaskId::new(1)
        );
        assert_eq!(
            task_id(11).expect("the twelfth task is number twelve"),
            TaskId::new(12)
        );
        let error =
            task_id(usize::MAX).expect_err("a position past the widest id cannot be named an id");
        assert_eq!(
            error.to_string(),
            format!(
                "corrupt data: task {} is past the {} a task id can hold",
                usize::MAX,
                u32::MAX
            )
        );
    }
}

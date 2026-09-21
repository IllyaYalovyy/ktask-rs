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
//!
//! [`validate`] is what asks that a queue entry is still complete. The four
//! required sections are the whole of the question, and it is asked of a task
//! already in the database as well as of a block being read now, so an import
//! and a lint cannot hold a task to two different standards.

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
///
/// An empty required field means the task lacks that section. Nothing here is
/// built so that it cannot happen — a row read out of the database holds what
/// the database held — so the question is asked by [`validate`] rather than by
/// the type.
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
    ///
    /// It is a section of the body and nothing else, so the queue's row holds no
    /// gate column, and a task read back out of the database has it read out of its
    /// body by `task::gate_of`.
    pub gate: Option<String>,
    /// The `**Protocol:**` section — the work protocol this task is worked with,
    /// as written.
    ///
    /// `None` is not a failed parse but the absence that lets a project's
    /// [`crate::Config::default_protocol`] answer; the chain that resolves the
    /// two words into phases is [`crate::protocol::for_task`], and `direct` is
    /// what a queue with nothing written anywhere is worked under.
    ///
    /// The word is checked here, when the task is added, rather than when an
    /// attempt starts: a task no build can work is not a queued task, and a
    /// run that discovered the fact twenty minutes in would have spent the
    /// attempt to report it ([`validate`] asks; the import, `plan lint` and the
    /// door to the queue all hold to that one answer). Unlike a gate, which the
    /// body alone carries, this fact has the column `docs/DESIGN.md` gives it,
    /// so a queue row stores the word instead of re-reading the block.
    pub protocol: Option<String>,
}

/// How many characters [`Task::title`] keeps before it cuts the line.
///
/// A count of characters rather than bytes, so the cut is the same for a task
/// written in any script: 80 Latin letters and 80 ideographs each fill the same
/// slot.
const TITLE_MAX_CHARS: usize = 80;

/// The label whose section makes a task a human gate.
const GATE_SECTION: &str = "Gate";

/// The label whose section names the work protocol a task is worked with.
const PROTOCOL_SECTION: &str = "Protocol";

/// The [`crate::Error::Config`] key a `**Protocol:**` naming no protocol is
/// refused under — the section's own word, lowercase, the way `provider` keys
/// the setting that names an adapter nobody has.
const PROTOCOL_KEY: &str = "protocol";

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
/// [`Task::gate`]; a `**Protocol:**` section is kept as [`Task::protocol`], and
/// is refused here if it names no protocol this build runs; the four required
/// sections are kept in their own fields, and any other bold label is kept in
/// the body alone.
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

/// Whether a task may be in the queue: every section the format requires, in
/// the field that section belongs to.
///
/// The queue holds a task once and re-checks it often — `add` asks before it
/// writes a row, `plan lint` asks of every row before anything runs — so this
/// is asked of the task, not of the document it came from. A block read from a
/// document is held to exactly this predicate by [`parse_plan`]: two standards
/// for one fact would mean a task that imports and then fails its own lint.
///
/// Nothing else is asked here. Whether a `Verify:` command parses and whether
/// a `Refs:` path exists belong to `plan lint` (docs/CONTRACT.md), and a
/// `**Gate:**` section is neither required nor excused: a gate is proved by a
/// person, but it still has to say what it asks them to approve.
///
/// # Errors
///
/// [`Error::Policy`] naming every required section the task lacks, not only
/// the first: a reader who has to run the check once per mistake learns that
/// the check cannot be trusted to have looked. No path is listed, because a
/// row of the queue broke the rule rather than a file. [`Error::Config`] keyed
/// `protocol` when the task's optional `**Protocol:**` section names no
/// protocol this build runs — which is why an unknown name never reaches an
/// attempt: the queue's door, [`crate::Journal::put_tasks`], asks this question
/// of every row before it writes one.
pub fn validate(task: &Task) -> Result<()> {
    let missing = missing_sections(task);
    if !missing.is_empty() {
        return Err(Error::Policy {
            detail: format!("task {} is missing {}", task.id, missing_phrase(&missing)),
            paths: Vec::new(),
        });
    }
    // Asked second, and of the same task: a block short of its required sections
    // has a shape to fix before anyone is told the word under its
    // `**Protocol:**` is unrunnable.
    check_protocol(task.protocol.as_deref(), &format!("task {}", task.id))
}

/// Ask that the word a `**Protocol:**` section holds names a protocol this build
/// runs, refusing it with `subject` — the task, or the block and line it came
/// from — written into the sentence a reader acts on.
///
/// The two rungs of the answer are the two a person can act on: a section with
/// nothing under it names no protocol and has to be filled in or deleted, and a
/// word that names nothing runnable is refused by quoting it back beside the two
/// words that would have worked. The names themselves come from
/// [`crate::protocol`], the one place they are spelled, so a refusal cannot
/// promise a protocol the build does not have.
///
/// # Errors
///
/// [`Error::Config`] keyed `protocol` naming the word, the two that exist, and
/// `subject`.
fn check_protocol(written: Option<&str>, subject: &str) -> Result<()> {
    let Some(text) = written else {
        return Ok(());
    };
    let name = text.trim();
    if name.is_empty() {
        return Err(Error::Config {
            key: PROTOCOL_KEY.to_owned(),
            detail: format!(
                "the `**{PROTOCOL_SECTION}:**` section of {subject} has nothing written under \
                 it, which names no work protocol: write {} or leave the section out to take \
                 the configured default",
                crate::protocol::alternatives(" or "),
            ),
        });
    }
    if crate::protocol::by_name(name).is_none() {
        return Err(Error::Config {
            key: PROTOCOL_KEY.to_owned(),
            detail: format!("{} — {subject}", crate::protocol::refusal(name)),
        });
    }
    Ok(())
}

/// The `**Gate:**` section a task body carries, if it carries one.
///
/// A gate is marked by this section of the body and by nothing else — not by a
/// status, and not by a column, because `docs/DESIGN.md` Database schema gives
/// the queue's `tasks` table no gate to hold. The body is therefore the one
/// home the fact has, and a task read back out of the database recovers it here
/// rather than storing a second copy that could disagree with the text it came
/// from (ADR-0019).
///
/// The body is read by the same scanner [`parse_plan`] reads a document by, so
/// the two rules a block and a row must never disagree about hold on both
/// paths: a `**Gate:**` inside a fenced code block marks nothing, and a label
/// written twice keeps the text written under it first.
pub(crate) fn gate_of(body: &str) -> Option<String> {
    section_of(body, GATE_SECTION)
}

/// The text of the `**Label:**` section `body` carries under `label`, if any.
///
/// The one reader of a block's labelled sections, so that every fact the
/// supervisor recovers out of a body — the gate [`gate_of`] recovers, and the
/// exception to test-first [`crate::protocol`] reads — is read by the scanner
/// [`parse_plan`] reads a document by. The two rules a block and a row must
/// never disagree about therefore hold on every path: a `**Label:**` inside a
/// fenced code block marks nothing, and a label written twice keeps the text
/// written under it first.
///
/// A label the body does not carry answers [`None`] — the absence a caller
/// distinguishes from a section that was written and left empty. An empty
/// section is a thing an author wrote, and is answered by whichever rule owns
/// that label, not by this one.
pub(crate) fn section_of(body: &str, label: &str) -> Option<String> {
    labelled_sections(&scan_lines(body)).get(label).cloned()
}

/// The id of the task at `index` in document order, counted from one.
///
/// The rule is shared with the queue: [`crate::Journal::put_tasks`] numbers the
/// rows it writes by this same count, so the position a block holds in a plan
/// and the id its row holds in the database are one fact and not two that
/// happen to agree (ADR-0019).
pub(crate) fn task_id(index: usize) -> Result<TaskId> {
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
            // A line outside a section is read and then dropped: `store`
            // empties the buffer before the text of the next label begins, so
            // whatever a line before any label put there never reaches a
            // section.
            None => text.push_str(written),
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
    let task = Task {
        id,
        status: TaskStatus::Pending,
        body: block_text(&block.lines),
        outcome: text_of(&sections, "Outcome"),
        done_when: text_of(&sections, "Done-when"),
        verify: text_of(&sections, "Verify"),
        refs: text_of(&sections, "Refs"),
        gate: sections.get(GATE_SECTION).cloned(),
        protocol: sections.get(PROTOCOL_SECTION).cloned(),
    };
    // The same predicate [`validate`] asks of a row already in the queue. The
    // report differs because the readers do: whoever holds this error is
    // holding the document, so the message gives them the line to open.
    let missing = missing_sections(&task);
    if !missing.is_empty() {
        return Err(Error::NotFound {
            what: missing_message(block, &missing),
        });
    }
    // The same question [`validate`] asks of a row, phrased for a reader who is
    // holding the document rather than the queue.
    check_protocol(
        task.protocol.as_deref(),
        &format!("`{}` (line {})", block.heading, block.starts_at),
    )
    .map(|()| task)
}

/// Each required section of a task, under the label the document writes it
/// under, in the order the format names them.
///
/// The labels are the ones [`build_task`] reads out of a block, so a section
/// has one name from the document to the queue.
fn required_sections(task: &Task) -> [(&'static str, &str); 4] {
    [
        ("Outcome", task.outcome.as_str()),
        ("Done-when", task.done_when.as_str()),
        ("Verify", task.verify.as_str()),
        ("Refs", task.refs.as_str()),
    ]
}

/// The required sections a task does not carry, in the order the format names
/// them, and every one of them rather than the first.
///
/// A label with nothing but whitespace under it is missing rather than empty.
/// A `**Verify:**` that holds nothing names no command to run, and the queue
/// keeps a task because something mechanical can be proved about it; an empty
/// `**Refs:**` answers the task to no document at all. A `**Gate:**` written
/// with nothing under it is different: that section marks the task one a
/// person must decide, so its presence is the fact and its text is a courtesy.
fn missing_sections(task: &Task) -> Vec<&'static str> {
    required_sections(task)
        .into_iter()
        .filter(|(_label, text)| text.trim().is_empty())
        .map(|(label, _text)| label)
        .collect()
}

/// The sections a task lacks, named as the document writes them, with the
/// noun that fits the count: one missing reads `the Refs: section`, two read
/// `the Verify: and Refs: sections`, and four are joined with commas before
/// the last.
fn missing_phrase(missing: &[&str]) -> String {
    let word = if missing.len() == 1 {
        "section"
    } else {
        "sections"
    };
    let mut named = String::new();
    for (index, label) in missing.iter().enumerate() {
        // The last name is joined with `and` and the ones before it with
        // commas, so a task short of all four reads as a list of them rather
        // than as four labels stacked up.
        if index > 0 {
            let before_the_last = index + 1 < missing.len();
            named.push_str(if before_the_last { ", " } else { " and " });
        }
        named.push('`');
        named.push_str(label);
        named.push_str(":`");
    }
    format!("the {named} {word}")
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
    format!(
        "{} of `{}` (line {})",
        missing_phrase(missing),
        block.heading,
        block.starts_at
    )
}

#[cfg(test)]
mod tests {
    use super::{Task, TaskStatus, parse_plan, task_id, validate};
    use crate::error::Error;
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
            protocol: None,
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
    fn a_task_that_carries_every_required_section_validates() {
        let task = task_with_first_line("Validate a task before it enters the queue");
        validate(&task).expect("a task with all four sections is queueable");
    }

    #[test]
    fn a_task_that_parses_also_validates() {
        let document = task_block("T060 Import and lint ask for the same four sections");
        let tasks = parse_plan(&document).expect("the block carries its four sections");
        let task = tasks.first().expect("one task was parsed");
        validate(task).expect("an imported task passes the check the queue re-runs");
    }

    #[test]
    fn a_task_missing_two_sections_reports_both_by_name() {
        let mut task = task_with_first_line("Reject a task that cannot be verified");
        task.done_when = String::new();
        task.verify = String::new();
        let error =
            validate(&task).expect_err("a task with no Done-when and no Verify proves nothing");
        assert!(
            matches!(error, Error::Policy { .. }),
            "{error} is a broken rule"
        );
        assert_eq!(
            error.to_string(),
            "policy violation: task 7 is missing the `Done-when:` and `Verify:` sections \
             (offending paths: )"
        );
    }

    #[test]
    fn a_task_missing_every_required_section_reports_all_four_by_name() {
        let mut task = task_with_first_line("Name every section a task lacks, not the first");
        task.outcome = String::new();
        task.done_when = String::new();
        task.verify = String::new();
        task.refs = String::new();
        let error = validate(&task).expect_err("a task with no sections at all cannot be queued");
        assert_eq!(
            error.to_string(),
            "policy violation: task 7 is missing the `Outcome:`, `Done-when:`, `Verify:` and \
             `Refs:` sections (offending paths: )"
        );
    }

    #[test]
    fn a_task_missing_one_section_names_that_one_section_in_the_singular() {
        let mut task = task_with_first_line("One missing section reads as one");
        task.refs = String::new();
        let error = validate(&task).expect_err("a task with no Refs is not answerable to anything");
        assert_eq!(
            error.to_string(),
            "policy violation: task 7 is missing the `Refs:` section (offending paths: )"
        );
    }

    #[test]
    fn a_required_section_whose_text_is_only_whitespace_is_missing() {
        // Every one of these renders as nothing on the page, and a `Verify:`
        // that renders as nothing verifies nothing.
        for blank in ["", " ", "\t \t", "\n", "\u{00a0}"] {
            let mut task = task_with_first_line("A blank section is a missing section");
            task.verify = blank.to_owned();
            let error = validate(&task).expect_err("an empty `Verify:` is no verification");
            assert_eq!(
                error.to_string(),
                "policy violation: task 7 is missing the `Verify:` section (offending paths: )"
            );
        }
    }

    #[test]
    fn a_required_label_with_nothing_written_under_it_fails_the_import() {
        let document = "\
## T061 A label with nothing under it

**Outcome:** a blank required section is a missing one.

**Done-when:** the import refuses the block, naming the label that holds nothing.

**Verify:**
   \t
**Refs:** docs/adr/0008
";
        let error = parse_plan(document).expect_err("an empty `Verify:` is no verification");
        assert_eq!(
            error.to_string(),
            "not found: the `Verify:` section of `## T061 A label with nothing under it` (line 1)"
        );
    }

    #[test]
    fn an_unknown_section_keeps_its_text_in_the_body_and_does_not_fail_validation() {
        let document = "\
## T062 A section the model does not keep

**Outcome:** a label the model does not name is content, not a defect.

**Do:** keep every label the model does not name.

**Done-when:** a task with an extra section still validates.

**Verify:** `cargo nextest run -p ktask-core`

**Refs:** VISION.md section 4
";
        let tasks = parse_plan(document).expect("an extra label is not a malformed task");
        let task = tasks.first().expect("one task was parsed");
        validate(task).expect("validation asks for the four required sections and no others");
        assert!(
            task.body
                .contains("**Do:** keep every label the model does not name."),
            "the section no field holds is still in the body it came from"
        );
    }

    #[test]
    fn a_gate_is_validated_by_the_same_sections_as_an_executable_task() {
        let mut task = task_with_first_line("A gate is a task, not an exemption");
        task.gate = Some("approve the migration".to_owned());
        validate(&task).expect("a gate still has to say how what it approves was proved");

        task.verify = String::new();
        let error = validate(&task).expect_err("a gate with no Verify asks to be trusted");
        assert_eq!(
            error.to_string(),
            "policy violation: task 7 is missing the `Verify:` section (offending paths: )"
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
    fn a_fence_closes_only_on_a_run_as_long_that_carries_nothing_else() {
        // A four-backtick fence quoting a three-backtick one, with a blank
        // line, a bold label and a run of backticks carrying text inside it.
        // This is the ordinary way to write about a fence, and the shape a
        // parser gets wrong when it closes a fence on any run of backticks, on
        // a shorter one, or on a blank line: the label inside the fence must
        // fill nothing, and the label after it must be the section.
        let document = "\
## T024 A task that quotes a fence

**Outcome:** a fence closes on its own delimiter and on nothing else.

**Done-when:** a test asserts that nothing inside the fence closed it.

**Verify:** `cargo nextest run -p ktask-core`

**Do:** every line of the fence below is content:

````markdown
```sh
cargo test
```

**Refs:** a label inside a fence fills nothing
```` tail carries text, so it is not a delimiter
````

**Refs:** VISION.md section 4
";
        let tasks = parse_plan(document).expect("a nested fence is one fence");
        assert_eq!(tasks.len(), 1, "a shorter run inside a fence opens nothing");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.body, document,
            "every backtick was kept where it was written"
        );
        assert_eq!(
            task.refs, "VISION.md section 4",
            "the label inside the fence is content, the one after it is the section"
        );
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

    /// The four required sections, so a protocol test differs from a complete
    /// task by one line and nothing else.
    const COMPLETE_TASK: &str = "\
## T075 Choose the protocol

**Outcome:** a task carries the protocol it is worked with.
**Done-when:** the choice is stored with the task.
**Verify:** `cargo nextest run -p ktask-core`
**Refs:** VISION.md section 9
";

    /// The refusal sentence a word that names no protocol earns, shared with
    /// the run-time refusal in `protocol.rs` so one fact has one wording.
    const NOT_A_PROTOCOL: &str = "`spec-first` is not a work protocol this build runs; \
the two it has are `direct` and `tdd`";

    #[test]
    fn a_protocol_section_names_the_protocol_the_task_is_worked_with() {
        let document = format!("{COMPLETE_TASK}\n**Protocol:** tdd\n");
        let tasks = parse_plan(&document).expect("a task may name the protocol it is worked with");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.protocol.as_deref(),
            Some("tdd"),
            "the word under `**Protocol:**` is kept as written, which is the word \
             `protocol::for_task` selects the phases by",
        );
        validate(task).expect("a task naming a protocol this build runs is a well-formed task");
        assert!(
            task.body.contains("**Protocol:** tdd"),
            "the section stays in the body it was written into: a supervisor imports a plan \
             and never edits it"
        );
    }

    #[test]
    fn a_task_with_no_protocol_section_names_no_protocol() {
        let tasks = parse_plan(COMPLETE_TASK).expect("the section is optional");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.protocol, None,
            "no section is not an empty choice: it is the absence that lets \
             `default_protocol` answer",
        );
    }

    #[test]
    fn a_protocol_section_written_twice_keeps_the_first_word() {
        let document = format!("{COMPLETE_TASK}\n**Protocol:** tdd\n\n**Protocol:** direct\n");
        let tasks = parse_plan(&document).expect("a repeated label is not a malformed task");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.protocol.as_deref(),
            Some("tdd"),
            "a label written twice holds the text written under it first, as every other \
             label in a task block does",
        );
    }

    #[test]
    fn a_protocol_label_inside_a_fence_names_no_protocol() {
        let document = format!("{COMPLETE_TASK}\n```markdown\n**Protocol:** spec-first\n```\n");
        let tasks =
            parse_plan(&document).expect("a label inside a fence is a line of somebody's example");
        let task = tasks.first().expect("one task was parsed");
        assert_eq!(
            task.protocol, None,
            "a quoted label is not a chosen protocol, and an example of a word this build \
             does not run must not fail the import it illustrates",
        );
    }

    #[test]
    fn a_protocol_section_with_nothing_under_it_fails_the_import() {
        let document = format!("{COMPLETE_TASK}\n**Protocol:**\n");
        let error = parse_plan(&document)
            .expect_err("a `**Protocol:**` that holds nothing names no protocol to run");
        let message = error.to_string();
        assert!(
            message.contains("protocol") && message.contains("names no work protocol"),
            "the refusal says which section is empty and what to write there: {message}"
        );
        assert!(
            message.contains("T075") || message.contains("line"),
            "whoever holds this error is holding the document, so it gives them the block: {message}"
        );
    }

    #[test]
    fn an_unknown_protocol_name_is_refused_when_the_task_is_added() {
        let document = format!("{COMPLETE_TASK}\n**Protocol:** spec-first\n");
        let error = parse_plan(&document)
            .expect_err("a word that names no protocol must not enter the queue");
        assert_eq!(
            error.to_string(),
            format!(
                "config error `protocol`: {NOT_A_PROTOCOL} — `## T075 Choose the protocol` \
                 (line 1)"
            ),
            "the refusal names the word it refused, the words that would have worked, and the \
             block to go back to"
        );
    }

    #[test]
    fn a_row_holding_an_unknown_protocol_never_validates() {
        // The same question `add` asks before it writes a row, asked of the row:
        // `plan lint` and the door to the queue cannot hold a task to two
        // standards.
        let mut task = parse_plan(COMPLETE_TASK)
            .expect("the scratch task is a task")
            .remove(0);
        task.protocol = Some("spec-first".to_owned());
        let error = validate(&task).expect_err("a row nobody can run is not a queue entry");
        assert_eq!(
            error.to_string(),
            format!("config error `protocol`: {NOT_A_PROTOCOL} — task 1"),
            "the refusal names the task the operator has to fix, in the words the import \
             refused it by"
        );
    }

    #[test]
    fn a_protocol_word_is_matched_exactly_as_it_is_written() {
        for written in ["Direct", "TDD", "direct ", " tdd"] {
            let document = format!("{COMPLETE_TASK}\n**Protocol:** {written}\n");
            let parsed = parse_plan(&document);
            if written.trim() == "direct" || written.trim() == "tdd" {
                assert!(
                    parsed.is_ok(),
                    "`{written}` is `{}` with whitespace around it, which the section \
                     trimming already removes: {parsed:?}",
                    written.trim(),
                );
                continue;
            }
            assert!(
                parsed.is_err(),
                "`{written}` is not a protocol this build runs and must not be folded into one",
            );
        }
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

#[cfg(test)]
/// Property tests: [`parse_plan`] is total, and it loses nothing.
///
/// The unit tests above assert the shapes a person has been seen to write.
/// These assert them over generated documents: arbitrary task counts, bodies in
/// any script, fenced code blocks, and lines that begin with `#` or `---`
/// (VISION.md §15). Three claims, in the order a reader should check them:
///
/// - **total** — any text at all produces tasks or an error, never a panic,
///   and whatever the answer is, it obeys the rules the queue is built on;
/// - **lossless** — for a document this test assembled out of parts it knows,
///   every task comes back with exactly the text written into it, and its body
///   is its block byte for byte;
/// - **durable** — what the queue keeps of a task is enough to read the task
///   back unchanged, text in any script included.
///
/// The database is a later task, so the third claim stores what the schema in
/// docs/DESIGN.md stores — the columns of the `tasks` table, as the bytes a
/// `TEXT` column holds — and nothing more.
mod properties {
    use proptest::collection::vec;
    use proptest::option;
    use proptest::prelude::*;
    use proptest::sample::select;
    use proptest::test_runner::TestCaseResult;
    use proptest::{prop_assert, prop_assert_eq, prop_oneof};

    use super::{
        Error, MAX_HEADING_INDENT, TITLE_MAX_CHARS, Task, TaskStatus, parse_plan, validate,
    };
    use crate::ids::TaskId;

    /// Every property in this module runs at least this many cases. Proptest's
    /// own default is the same number; writing it down is what turns "at least
    /// 256 cases" from a question about someone's configuration into one this
    /// file answers.
    const CASES: u32 = 256;

    /// The section labels the queue keeps a field for, and the only labels a
    /// generated line may never begin with.
    const KEPT_LABELS: &[&str] = &["Outcome", "Done-when", "Verify", "Refs", "Gate", "Protocol"];

    /// The characters a fence may be made of.
    const MARKERS: &[char] = &['`', '~'];

    /// The language tag that may follow a fence's opening run.
    const INFO_TAGS: &[&str] = &["", "rust", "sh", "markdown"];

    /// What comes before the first task: read, then kept out of the queue. It
    /// holds a rule, a heading, a fence with a heading and a label inside it,
    /// and a label of its own, so a preamble cannot be mistaken for a task.
    const PREAMBLE: &str = "\
# The plan

---

```
## not a task: a heading inside a fence
**Outcome:** a preamble is not a task
```

**Outcome:** what this document is for, which no task inherits.

";

    /// Whole phrases, in no script in particular: Latin, Cyrillic, Han, Kana,
    /// Hangul, Hebrew, Greek, emoji, a flag sequence, a no-break space, a
    /// combining accent, a zero-width joiner and a tab inside a line. A parser
    /// that is only correct for ASCII is the bug this module exists to fail on,
    /// and text that has to survive a byte-for-byte comparison is only worth
    /// generating if some of it is multi-byte.
    ///
    /// None of them begins or ends with whitespace, holds a line break, or
    /// holds a backtick, tilde or asterisk — so a line written out of these
    /// says exactly what the round-trip property then expects of it, and two of
    /// them plus a heading marker still fit inside the title limit.
    const FRAGMENTS: &[&str] = &[
        "the queue holds a task",
        "черновик задачи",
        "タイトルを保持する",
        "한국어 작업 항목",
        "עברית של משימה",
        "Ελληνικά κείμενα",
        "😀👩‍👩‍👧‍👦 emoji",
        "🇯🇵 a flag sequence",
        "e\u{301} combining accent",
        "zero\u{200d}width joiner",
        "a\u{a0}no-break space",
        "tab\tinside a line",
        "#1 приоритет",
        "crates/ktask-core/src/task.rs",
        "cargo nextest run -p core",
        "Привет, мир",
        "Ünïcödé",
        "—",
    ];

    /// Lines a plan holds besides its sections, and the ones a parser that
    /// reserves a line prefix misreads: a rule, a heading of the document, a
    /// subheading inside the task above it, a level-two heading indented too
    /// deep to be one, `##` with no space after it, a label the queue does not
    /// keep, and prose in another script.
    ///
    /// None opens a task, opens a fence, or opens a kept section, so a block
    /// that they follow is still one block with the same sections.
    /// [`the_generated_pieces_stay_clear_of_the_markup`] is what keeps them so.
    const DOCUMENT_LINES: &[&str] = &[
        "",
        "---",
        "- - -",
        "___",
        "# The plan, as a person wrote it",
        "### A step inside the task",
        "##NoSpace is prose, not a heading",
        "    ## four spaces deep is a code block",
        "**Files:** crates/ktask-core/src/task.rs",
        "См. VISION.md §15",
        "1. first, prove it fails",
        "日本語の注釈",
    ];

    /// What a backtick fence may hide without ending itself: a level-two
    /// heading, every label the queue keeps, rules, and delimiter runs of the
    /// other marker or shorter runs of its own. A fence closes only on a line
    /// that is its own run and nothing else, so everything that would close a
    /// three-deep backtick fence is in the tilde list instead, and the reverse.
    const BACKTICK_FENCE_LINES: &[&str] = &[
        "## a heading inside a fence opens no task",
        "**Outcome:** a label inside a fence fills no section",
        "**Done-when:** nothing",
        "**Verify:** nothing",
        "**Refs:** nothing",
        "**Gate:** nobody is asked to decide",
        "---",
        "#",
        "``",
        "~~~",
        "~~~~",
        "text with ``` and ~~~ inside it",
        "コードブロック内のテキスト 🤖",
        "\tindented, and still content",
    ];

    /// The same, for a fence opened with tildes.
    const TILDE_FENCE_LINES: &[&str] = &[
        "## a heading inside a fence opens no task",
        "**Outcome:** a label inside a fence fills no section",
        "**Gate:** nobody is asked to decide",
        "---",
        "#",
        "~~",
        "```",
        "````",
        "text with ``` and ~~~ inside it",
        "コードブロック内のテキスト 🤖",
    ];

    /// Every line shape the totality properties throw at the parser, including
    /// the delimiters and headings that move a block when they are read where
    /// they should not be.
    const SOUP_LINES: &[&str] = &[
        "",
        "#",
        "##",
        "## a task",
        "### deeper",
        "#### deeper still",
        "\t## a tab-indented heading",
        "   ## three spaces is still a heading",
        "    ## four spaces is not",
        "---",
        "- - -",
        "***",
        "```",
        "```rust",
        "````",
        "~~~",
        "~~",
        "`",
        "**Outcome:** filled",
        "**Outcome:**",
        "**Gate:**",
        "**Done-when:** filled",
        "**Refs:** filled",
        "**Verify:** cargo nextest run",
        "the queue holds a task",
        "**Outcome:** the queue holds a task",
        "**Refs:** VISION.md §15",
        "См. VISION.md §15",
        "😀😀😀",
        "e\u{301}e\u{301}",
        "\u{a0}\u{200d}",
        "\u{0} a control character",
        "\r",
        "a very long line that goes on and on and on past any width a screen has",
        "## a heading far past the eighty character limit a title keeps, which is \
         the shape a queue list has to cut to fit its pane\n**Outcome:** a title is \
         cut, the body is not\n**Done-when:** a test asserts the cut\n**Verify:** \
         cargo nextest run\n**Refs:** docs/adr/0006",
    ];

    /// How the lines of a generated document are joined. A file with no final
    /// break and a file written on Windows are both ordinary; a parser that
    /// counts by byte offset finds out here which one it is.
    const LINE_JOINS: &[&str] = &["\n", "\r\n", "\n\n", "\n\r"];

    /// Text for one line: one to four fragments, which is never empty, never
    /// only whitespace, and never needs trimming to come back as written.
    fn line_text() -> impl Strategy<Value = String> {
        vec(select(FRAGMENTS), 1..4).prop_map(|parts| parts.join(" "))
    }

    /// The lines between a heading and the first section of its block.
    fn document_noise() -> impl Strategy<Value = Vec<&'static str>> {
        vec(select(DOCUMENT_LINES), 0..5)
    }

    /// The lines a fence may hide, chosen for the marker that opened it.
    fn hidden_lines(marker: char) -> &'static [&'static str] {
        if marker == '`' {
            BACKTICK_FENCE_LINES
        } else {
            TILDE_FENCE_LINES
        }
    }

    /// A fenced code block: its delimiter character, how deep the run is, the
    /// tag after it, and the lines it hides.
    #[derive(Clone, Debug)]
    struct Fence {
        marker: char,
        run: usize,
        info: &'static str,
        hidden: Vec<&'static str>,
    }

    impl Fence {
        /// The fence as it is written, both delimiters included.
        fn render(&self) -> String {
            let delimiter = self.marker.to_string().repeat(self.run);
            let mut text = format!("{delimiter}{}\n", self.info);
            for line in &self.hidden {
                text.push_str(line);
                text.push('\n');
            }
            text.push_str(&delimiter);
            text.push('\n');
            text
        }
    }

    /// A fence, with a body that cannot close it.
    fn fence() -> impl Strategy<Value = Fence> {
        (select(MARKERS), 3..6usize, select(INFO_TAGS))
            .prop_flat_map(|(marker, run, info)| {
                (
                    Just(marker),
                    Just(run),
                    Just(info),
                    vec(select(hidden_lines(marker)), 1..5),
                )
            })
            .prop_map(|(marker, run, info, hidden)| Fence {
                marker,
                run,
                info,
                hidden,
            })
    }

    /// The text a `**Gate:**` section may hold: no section at all, an empty
    /// one, or one that says what a person is being asked to decide.
    fn gate_text() -> impl Strategy<Value = Option<String>> {
        prop_oneof![
            Just(None),
            Just(Some(String::new())),
            line_text().prop_map(Some),
        ]
    }

    /// One task block as this test writes it: the four required sections and
    /// the optional gate, plus the parts whose only job is to be in the way.
    #[derive(Clone, Debug)]
    struct Spec {
        heading: String,
        noise: Vec<&'static str>,
        fence: Option<Fence>,
        outcome: String,
        done_when: String,
        verify: String,
        refs: String,
        gate: Option<String>,
        protocol: Option<&'static str>,
        blanks: usize,
    }

    impl Spec {
        /// The block exactly as it will be written, which is what the parsed
        /// task's body has to come back as.
        fn render(&self) -> String {
            let mut text = format!("## {}\n", self.heading);
            for line in &self.noise {
                text.push_str(line);
                text.push('\n');
            }
            text.push('\n');
            if let Some(fence) = &self.fence {
                text.push_str(&fence.render());
                text.push('\n');
            }
            write_a_section(&mut text, "Outcome", &self.outcome);
            write_a_section(&mut text, "Done-when", &self.done_when);
            write_a_section(&mut text, "Verify", &self.verify);
            write_a_section(&mut text, "Refs", &self.refs);
            if let Some(protocol) = self.protocol {
                write_a_section(&mut text, "Protocol", protocol);
            }
            if let Some(gate) = &self.gate {
                // A `**Gate:**` with nothing under it is written that way: the
                // section is the fact, its text is a courtesy.
                if gate.is_empty() {
                    text.push_str("**Gate:**\n");
                } else {
                    write_a_section(&mut text, "Gate", gate);
                }
            }
            for _ in 0..self.blanks {
                text.push('\n');
            }
            text
        }

        /// The title the parsed task should report: the heading line, which is
        /// never long enough here to reach the limit that cuts a title.
        fn title(&self) -> String {
            format!("## {}", self.heading)
        }
    }

    /// One section as a person writes it: the label, a space, the text, the
    /// break. Pushed in pieces rather than formatted, because the point of the
    /// round trip is that nothing between them is rewritten.
    fn write_a_section(text: &mut String, label: &str, value: &str) {
        text.push_str("**");
        text.push_str(label);
        text.push_str(":** ");
        text.push_str(value);
        text.push('\n');
    }

    /// The word a `**Protocol:**` section may hold: no section at all, or one of
    /// the two names a build runs. No name that is not in
    /// [`crate::protocol::names`] may be generated, because every generated task
    /// is asserted to validate.
    fn protocol_word() -> impl Strategy<Value = Option<&'static str>> {
        prop_oneof![Just(None), Just(Some("direct")), Just(Some("tdd")),]
    }

    /// One task spec: a heading of one or two fragments, noise, maybe a fence,
    /// the four sections, maybe a protocol, maybe a gate, and the blank lines
    /// that trail it.
    fn spec() -> impl Strategy<Value = Spec> {
        (
            vec(select(FRAGMENTS), 1..3),
            document_noise(),
            option::of(fence()),
            line_text(),
            line_text(),
            line_text(),
            line_text(),
            gate_text(),
            protocol_word(),
            0..3usize,
        )
            .prop_map(
                |(
                    heading,
                    noise,
                    fence,
                    outcome,
                    done_when,
                    verify,
                    refs,
                    gate,
                    protocol,
                    blanks,
                )| Spec {
                    heading: heading.join(" "),
                    noise,
                    fence,
                    outcome,
                    done_when,
                    verify,
                    refs,
                    gate,
                    protocol,
                    blanks,
                },
            )
    }

    /// A whole plan document and the specs it was built from: the preamble a
    /// person reads, then the task blocks back to back, so the task count is
    /// whatever the generator chose, zero included.
    fn document_and_specs() -> impl Strategy<Value = (String, Vec<Spec>)> {
        vec(spec(), 0..7).prop_map(|specs| {
            let blocks: String = specs.iter().map(Spec::render).collect();
            (format!("{PREAMBLE}{blocks}"), specs)
        })
    }

    /// A document assembled out of the shapes that confuse a
    /// format-reserving parser, joined every way a file gets joined.
    fn plan_soup() -> impl Strategy<Value = String> {
        (
            vec(select(SOUP_LINES), 0..40),
            select(LINE_JOINS),
            any::<bool>(),
        )
            .prop_map(|(lines, join, final_break)| {
                let mut text = lines.join(join);
                if final_break {
                    text.push_str(join);
                }
                text
            })
    }

    /// What every answer of [`parse_plan`] must satisfy, whatever it was given.
    ///
    /// Reaching the end of this function is the assertion that parsing did not
    /// panic: a panic is a failed case, and proptest shrinks the text that
    /// provokes it down to the smallest text that still panics.
    fn check_total(text: &str, parsed: Result<Vec<Task>, Error>) -> TestCaseResult {
        let tasks = match parsed {
            Ok(tasks) => tasks,
            Err(error) => {
                let message = error.to_string();
                prop_assert!(
                    matches!(error, Error::NotFound { .. }),
                    "a plan is refused only for a section it lacks, got: {message}"
                );
                prop_assert!(
                    !message.is_empty(),
                    "a refusal that names nothing helps nobody"
                );
                return Ok(());
            }
        };
        for (index, task) in tasks.iter().enumerate() {
            let position = u32::try_from(index + 1)
                .expect("a generated document cannot hold more tasks than an id names");
            prop_assert_eq!(
                task.id,
                TaskId::new(position),
                "ids count from one in the order the tasks are written"
            );
            prop_assert_eq!(
                task.status,
                TaskStatus::Pending,
                "an import concludes nothing, so no task can be Done yet"
            );
            prop_assert!(
                validate(task).is_ok(),
                "a task that imported is a task that validates: {task:?}"
            );
            prop_assert!(
                !task.body.is_empty(),
                "a task that exists has a block: {:?}",
                task.id
            );
            prop_assert!(
                task.body.starts_with(task.title()) && !task.title().is_empty(),
                "a title is a non-empty prefix of the body it is read out of: {:?}",
                task.title()
            );
            prop_assert!(
                task.title().chars().count() <= TITLE_MAX_CHARS,
                "a title is cut to the limit, got {:?}",
                task.title()
            );
            prop_assert!(
                task.title()
                    .trim_start_matches([' ', '\t'])
                    .starts_with("##"),
                "only a level-two heading opens a task: {:?}",
                task.title()
            );
            prop_assert!(
                task.gate.is_none() || task.body.contains("**Gate:"),
                "a gate is read out of a `**Gate:**` section and nowhere else: {task:?}"
            );
        }
        assert_blocks_are_the_document(text, &tasks)
    }

    /// Reading the blocks back is reading the document.
    ///
    /// A block is its lines and nothing else, so the blocks of a document,
    /// concatenated, are that document from its first heading onward — every
    /// byte of it, fence delimiters, line breaks and rules included — and
    /// parsing that again gives the same tasks in the same order. A parser that
    /// stripped a line, rewrote a break, or merged two blocks fails here.
    fn assert_blocks_are_the_document(text: &str, tasks: &[Task]) -> TestCaseResult {
        let mut kept = String::new();
        for task in tasks {
            kept.push_str(&task.body);
        }
        prop_assert!(
            text.ends_with(&kept),
            "the tasks are the document from its first heading on with nothing \
             stripped: the kept blocks are {} bytes and the document is {} bytes, \
             and the kept text is not the end of it",
            kept.len(),
            text.len()
        );
        prop_assert_eq!(
            parse_plan(&kept).ok(),
            Some(tasks.to_vec()),
            "reading the kept blocks again gives the same tasks"
        );
        Ok(())
    }

    /// The bytes a column holds, read back as text. What the database hands
    /// back is UTF-8, and text that did not survive the trip would fail here
    /// rather than print mojibake on the Task detail screen.
    fn column_text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("a TEXT column holds UTF-8")
    }

    /// What the `tasks` table of docs/DESIGN.md keeps of a task: its columns,
    /// as the bytes a `TEXT` column holds.
    ///
    /// There is deliberately no `gate` column, because the schema has none —
    /// ADR-0007 leaves it to the task that stores the queue to add one or to
    /// re-derive the section from the body. So the columns are read back
    /// without a gate, and the stored body is read back with everything, which
    /// is the claim that holds either way: the body the queue keeps is enough
    /// to reproduce the task. `protocol` *is* a column, and so is kept: an
    /// absent protocol is the `NULL` the schema comments "means the configured
    /// default", which is a fact a read has to be able to tell apart from a
    /// word that went missing.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct StoredTask {
        id: u32,
        title: Vec<u8>,
        outcome: Vec<u8>,
        done_when: Vec<u8>,
        verify: Vec<u8>,
        refs: Vec<u8>,
        protocol: Option<Vec<u8>>,
        body: Vec<u8>,
    }

    impl StoredTask {
        /// The row an import writes.
        fn store(task: &Task) -> Self {
            Self {
                id: task.id.get(),
                title: task.title().as_bytes().to_vec(),
                outcome: task.outcome.as_bytes().to_vec(),
                done_when: task.done_when.as_bytes().to_vec(),
                verify: task.verify.as_bytes().to_vec(),
                refs: task.refs.as_bytes().to_vec(),
                protocol: task
                    .protocol
                    .as_ref()
                    .map(String::as_bytes)
                    .map(<[u8]>::to_vec),
                body: task.body.as_bytes().to_vec(),
            }
        }

        /// The task those columns describe, in the state an import leaves it.
        fn read_back(&self) -> Task {
            Task {
                id: TaskId::new(self.id),
                status: TaskStatus::Pending,
                body: column_text(&self.body),
                outcome: column_text(&self.outcome),
                done_when: column_text(&self.done_when),
                verify: column_text(&self.verify),
                refs: column_text(&self.refs),
                gate: None,
                protocol: self.protocol.as_ref().map(|bytes| column_text(bytes)),
            }
        }

        /// The task re-read by parsing the stored body, which is where a gate
        /// comes from when there is no column to hold it.
        fn read_back_from_body(&self) -> Task {
            let body = column_text(&self.body);
            let mut reread = parse_plan(&body).unwrap_or_else(|error| {
                panic!("a block read back from the store is still a plan: {error}")
            });
            assert_eq!(reread.len(), 1, "one block on its own is one task");
            reread.remove(0)
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(CASES))]

        /// Any text at all, in any script, arranged however it likes: the
        /// parser answers or refuses, and never panics doing it.
        #[test]
        fn any_text_at_all_becomes_tasks_or_an_error_but_never_a_panic(text in any::<String>()) {
            check_total(&text, parse_plan(&text))?;
        }

        /// The same claim over the text a plan document is actually made of,
        /// where a line is as likely to begin with `---`, `#`, a fence run or a
        /// bold label as with words.
        #[test]
        fn the_lines_a_plan_is_made_of_become_tasks_or_an_error(text in plan_soup()) {
            check_total(&text, parse_plan(&text))?;
        }

        /// A document this test wrote, part for known part: every task comes
        /// back with the sections that were written into it, its body is its
        /// block byte for byte, and what the queue keeps of it is enough to
        /// read the task back unchanged.
        #[test]
        fn a_generated_document_returns_the_tasks_that_were_written_into_it(
            (document, specs) in document_and_specs(),
        ) {
            let tasks = parse_plan(&document)
                .unwrap_or_else(|error| panic!("a generated plan is well formed: {error}"));
            prop_assert_eq!(
                tasks.len(),
                specs.len(),
                "every block is one task, and a heading or a label inside a \
                 fence, a rule or a `#` line opens nothing"
            );
            let mut rows = Vec::with_capacity(tasks.len());
            for (index, (task, spec)) in tasks.iter().zip(&specs).enumerate() {
                let position = u32::try_from(index + 1)
                    .expect("a generated document cannot hold more tasks than an id names");
                prop_assert_eq!(
                    task.id,
                    TaskId::new(position),
                    "ids count from one in the order the tasks are written"
                );
                prop_assert_eq!(&task.body, &spec.render(), "the block is kept verbatim");
                prop_assert_eq!(&task.outcome, &spec.outcome);
                prop_assert_eq!(&task.done_when, &spec.done_when);
                prop_assert_eq!(&task.verify, &spec.verify);
                prop_assert_eq!(&task.refs, &spec.refs);
                prop_assert_eq!(
                    &task.gate,
                    &spec.gate,
                    "a gate is a `**Gate:**` section, kept with what it asks"
                );
                prop_assert_eq!(
                    &task.protocol,
                    &spec.protocol.map(str::to_owned),
                    "a `**Protocol:**` section is kept as the word it holds, and its absence \
                     stays an absence"
                );
                prop_assert_eq!(task.status, TaskStatus::Pending);
                prop_assert_eq!(task.title(), spec.title(), "the title is the heading line");
                prop_assert!(validate(task).is_ok(), "a task that imported validates");
                rows.push(StoredTask::store(task));
            }
            for (task, row) in tasks.iter().zip(&rows) {
                let columns = row.read_back();
                prop_assert_eq!(
                    &columns,
                    &Task { gate: None, ..task.clone() },
                    "the columns hold the task, protocol included and text in any script \
                     included"
                );
                prop_assert_eq!(
                    column_text(&row.title),
                    columns.title().to_owned(),
                    "the stored title is still the projection of the stored body"
                );
                let reread = row.read_back_from_body();
                prop_assert_eq!(
                    Task { id: task.id, ..reread },
                    task.clone(),
                    "the stored body reproduces the whole task, gate and protocol included"
                );
            }
            assert_blocks_are_the_document(&document, &tasks)?;
        }
    }

    #[test]
    fn the_generated_pieces_stay_clear_of_the_markup_that_would_move_a_block() {
        // The exactness the round-trip property claims rests on what these
        // pools are allowed to hold. Asserted rather than assumed: a fragment
        // that gained a line break, a fence run, or a space at either end
        // tomorrow would make the property's expectations wrong, and a wrong
        // expectation fails on some generated document instead of here.
        for fragment in FRAGMENTS {
            assert!(!fragment.is_empty(), "a fragment has to say something");
            assert!(
                !fragment.chars().any(|ch| ch == '\n' || ch == '\r'),
                "a fragment is one line: {fragment:?}"
            );
            assert!(
                !fragment.contains(['`', '~', '*']),
                "a fragment opens no fence and no label: {fragment:?}"
            );
            assert!(
                fragment.chars().count() * 2 + 4 <= TITLE_MAX_CHARS,
                "two fragments, a space and a heading marker still fit a title: {fragment:?}"
            );
            assert!(
                fragment
                    .chars()
                    .next()
                    .is_some_and(|ch| !ch.is_whitespace())
                    && fragment
                        .chars()
                        .last()
                        .is_some_and(|ch| !ch.is_whitespace()),
                "a fragment that needed trimming would not come back as written: {fragment:?}"
            );
        }
        for line in DOCUMENT_LINES {
            assert!(
                !line.contains(['`', '~']),
                "a document line opens no fence: {line:?}"
            );
            let text = line.trim_start_matches([' ', '\t']);
            let indented_past_a_heading = line.len() - text.len() > MAX_HEADING_INDENT;
            let level_two = text.starts_with("## ") || text == "##";
            assert!(
                !level_two || indented_past_a_heading,
                "a level-two heading outside a fence would open a second task: {line:?}"
            );
            assert!(
                !opens_a_kept_label(text),
                "a kept label ahead of the sections would fill one: {line:?}"
            );
        }
        for (marker, lines) in [('`', BACKTICK_FENCE_LINES), ('~', TILDE_FENCE_LINES)] {
            for line in lines {
                assert!(
                    !line.contains(['\n', '\r']),
                    "a fence holds one line at a time: {line:?}"
                );
                let run = line.chars().take_while(|ch| *ch == marker).count();
                let only_the_marker = !line.is_empty() && line.chars().all(|ch| ch == marker);
                assert!(
                    run < 3 || !only_the_marker,
                    "{line:?} would close a {marker} fence three deep, so what \
                     came after it would stop being hidden"
                );
            }
        }
        for label in KEPT_LABELS {
            assert!(
                !FRAGMENTS.iter().any(|fragment| fragment.contains(label)),
                "a fragment holding {label} would be a label in disguise"
            );
        }
    }

    /// Whether a line opens one of the sections the queue keeps a field for.
    fn opens_a_kept_label(line: &str) -> bool {
        KEPT_LABELS
            .iter()
            .any(|label| line.starts_with(format!("**{label}:**").as_str()))
    }

    #[test]
    fn a_preamble_holds_a_rule_a_heading_and_a_label_and_becomes_no_task() {
        // Every generated document opens with this preamble, so the claim that
        // a preamble is read and then kept out of the queue is stated once, in
        // prose, rather than resting only on a property that happens to pass.
        let document = format!(
            "{PREAMBLE}## a task\n\n**Outcome:** one.\n**Done-when:** asserted.\n\
             **Verify:** `cargo nextest run`\n**Refs:** VISION.md §15\n"
        );
        let mut tasks = parse_plan(&document).expect("the block after the preamble is a task");
        assert_eq!(
            tasks.len(),
            1,
            "the preamble's rule, fence and label queued nothing"
        );
        let task = tasks.remove(0);
        assert_eq!(task.outcome, "one.");
        assert_eq!(task.refs, "VISION.md §15");
        assert!(
            !task.body.contains("# The plan"),
            "the preamble stays in the preamble"
        );
    }
}

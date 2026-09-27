//! Adding a task by writing it in an editor: the template, reading it back, and the use case.

use std::error::Error;
use std::fmt;

use crate::task::draft_problems;
use crate::{AddError, Clock, Journal, Placement, Task, TaskDraft, TaskKind, TaskStatus, add_task};

/// The text an editor is opened on. Everything above the `Title:` line is ignored.
const TEMPLATE: &str = "\
# Describe the task below, then save and quit. Nothing is added if the file is left
# unchanged or emptied. Everything above the \"Title:\" line is ignored.
#
# A task needs a title and at least one criterion. Kind is agent or human. Links and
# Criteria take one item per line, each starting with \"- \". A link is
# github:owner/repo#NUMBER or an http(s) URL. The body may run over many lines; it ends at
# the \"Criteria:\" line.

Title:
Kind: agent
Links:
-
Body:

Criteria:
-
";

/// Why the editor could not give a text back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorError {
    message: String,
}

impl EditorError {
    /// An error described by `message`, which names what failed and why.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for EditorError {}

/// Port: the person's text editor.
pub trait Editor {
    /// Lets the person edit `text` and returns what they left.
    ///
    /// # Errors
    ///
    /// Fails when the editor cannot be run, exits unsuccessfully, or leaves something that
    /// is not text.
    fn edit(&self, text: &str) -> Result<String, EditorError>;
}

/// One thing wrong with what the editor returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateProblem {
    /// The `Kind:` section holds something that is not a kind; the text says what is.
    Kind(String),
    /// A section that takes one line has more.
    ExtraLines(&'static str),
    /// The task the template describes breaks a rule.
    Task(AddError),
}

impl fmt::Display for TemplateProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kind(message) => write!(f, "Kind: {message}"),
            Self::ExtraLines(section) => write!(f, "{section}: takes one line, and there is more"),
            Self::Task(error) => error.fmt(f),
        }
    }
}

/// Why no task was added through the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditAddError {
    /// The editor did not give a text back.
    Editor(EditorError),
    /// The text breaks the template's rules; every problem is listed.
    Invalid(Vec<TemplateProblem>),
    /// The task could not be added.
    Add(AddError),
}

impl fmt::Display for EditAddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Editor(error) => error.fmt(f),
            Self::Invalid(problems) => {
                write!(f, "the task is not valid, so nothing was added")?;
                problems
                    .iter()
                    .try_for_each(|problem| write!(f, "\n  - {problem}"))
            }
            Self::Add(error) => error.fmt(f),
        }
    }
}

impl Error for EditAddError {}

/// What came of opening the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edited {
    /// The template was filled in and the task added.
    Added(Task),
    /// The template was left as it was.
    Unchanged,
    /// The text was emptied.
    Emptied,
}

/// The section of the template a line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Preamble,
    Title,
    Kind,
    Links,
    Body,
    Criteria,
}

impl Section {
    const NAMED: [(&'static str, Self); 5] = [
        ("Title", Self::Title),
        ("Kind", Self::Kind),
        ("Links", Self::Links),
        ("Body", Self::Body),
        ("Criteria", Self::Criteria),
    ];

    fn name(self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(_, section)| *section == self)
            .map_or("", |(name, _)| name)
    }

    /// The section that `line` starts, and what follows its name, if it starts one. The body
    /// runs until `Criteria:`, so inside it nothing else starts a section.
    fn started_by(line: &str, current: Self) -> Option<(Self, &str)> {
        let (label, rest) = line.split_once(':')?;
        Self::NAMED
            .iter()
            .find(|(name, section)| {
                label.eq_ignore_ascii_case(name)
                    && (current != Self::Body || *section == Self::Criteria)
            })
            .map(|(_, section)| (*section, rest))
    }
}

/// The lines under each section.
#[derive(Debug, Default)]
struct Sections<'a> {
    title: Vec<&'a str>,
    kind: Vec<&'a str>,
    links: Vec<&'a str>,
    body: Vec<&'a str>,
    criteria: Vec<&'a str>,
    ignored: Vec<&'a str>,
}

impl<'a> Sections<'a> {
    fn of(text: &'a str) -> Self {
        let mut sections = Self::default();
        let mut current = Section::Preamble;
        for line in text.lines() {
            if let Some((section, rest)) = Section::started_by(line, current) {
                current = section;
                sections.lines(section).extend(Some(rest));
            } else {
                sections.lines(current).extend(Some(line));
            }
        }
        sections
    }

    /// Where the lines of `section` go; the preamble's are dropped.
    fn lines(&mut self, section: Section) -> &mut Vec<&'a str> {
        match section {
            Section::Preamble => &mut self.ignored,
            Section::Title => &mut self.title,
            Section::Kind => &mut self.kind,
            Section::Links => &mut self.links,
            Section::Body => &mut self.body,
            Section::Criteria => &mut self.criteria,
        }
    }
}

/// The non-blank lines of a section, trimmed.
fn filled<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The items of a list section: one per non-blank line, without the leading `-`.
fn items(lines: &[&str]) -> Vec<String> {
    filled(lines)
        .into_iter()
        .map(|line| line.strip_prefix('-').unwrap_or(line).trim())
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The one line of a section that takes one line, noting when there is more.
fn single<'a>(section: Section, lines: &[&'a str], problems: &mut Vec<TemplateProblem>) -> &'a str {
    let lines = filled(lines);
    if lines.len() > 1 {
        problems.push(TemplateProblem::ExtraLines(section.name()));
    }
    lines.first().copied().unwrap_or_default()
}

/// The task a filled-in template describes, and what is wrong with how it is written.
fn parse(text: &str) -> (TaskDraft, Vec<TemplateProblem>) {
    let Sections {
        title,
        kind,
        links,
        body,
        criteria,
        ..
    } = Sections::of(text);
    let mut problems = Vec::new();
    let title = single(Section::Title, &title, &mut problems).to_owned();
    let kind = match single(Section::Kind, &kind, &mut problems) {
        "" => TaskKind::default(),
        text => text.parse().unwrap_or_else(|message| {
            problems.push(TemplateProblem::Kind(message));
            TaskKind::default()
        }),
    };
    // Blank lines around the body are layout; the lines inside it are kept as written.
    let mut body: Vec<&str> = body.iter().map(|line| line.trim_end()).collect();
    while body.last() == Some(&"") {
        body.pop();
    }
    let leading = body.iter().take_while(|line| line.is_empty()).count();
    let body = body.split_off(leading).join("\n");
    let draft = TaskDraft {
        title,
        body,
        criteria: items(&criteria),
        kind,
        links: items(&links),
    };
    (draft, problems)
}

/// `text` without trailing blanks on its lines or its end, so that an editor's habit of
/// adding a final newline is not a change.
fn normalized(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Fails when `placement` names a task a new one cannot go next to.
fn check_placement(journal: &impl Journal, placement: Placement) -> Result<(), AddError> {
    let (Placement::Before(anchor) | Placement::After(anchor)) = placement else {
        return Ok(());
    };
    let tasks = journal.tasks().map_err(AddError::Journal)?;
    match tasks.iter().find(|task| task.id == anchor) {
        None => Err(AddError::UnknownTask(anchor)),
        Some(task) if task.status == TaskStatus::Cancelled => Err(AddError::CancelledTask(anchor)),
        Some(_) => Ok(()),
    }
}

/// Use case: opens the editor on the template and adds the task written in it at
/// `placement`.
///
/// Leaving the template unchanged or emptying it adds nothing and is not an error.
///
/// # Errors
///
/// Fails, adding nothing, when `placement` names a task that does not exist or was
/// cancelled (checked before the editor opens, so no writing is lost), when the editor
/// fails, when the text is not a valid task — every problem is listed — or when the journal
/// cannot be written.
pub fn add_task_in_editor(
    journal: &impl Journal,
    clock: &impl Clock,
    editor: &impl Editor,
    placement: Placement,
) -> Result<Edited, EditAddError> {
    check_placement(journal, placement).map_err(EditAddError::Add)?;
    let text = editor.edit(TEMPLATE).map_err(EditAddError::Editor)?;
    if text.trim().is_empty() {
        return Ok(Edited::Emptied);
    }
    if normalized(&text) == normalized(TEMPLATE) {
        return Ok(Edited::Unchanged);
    }
    let (draft, mut problems) = parse(&text);
    problems.extend(
        draft_problems(&draft)
            .into_iter()
            .map(TemplateProblem::Task),
    );
    if !problems.is_empty() {
        return Err(EditAddError::Invalid(problems));
    }
    add_task(journal, clock, &draft, placement)
        .map(Edited::Added)
        .map_err(EditAddError::Add)
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeEditor, FakeJournal, at, draft};
    use crate::{JournalError, TaskId, list_all_tasks, list_tasks};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(500))
    }

    /// Runs the use case on an empty journal with an editor that leaves `text`.
    fn add_written(text: &str) -> (Result<Edited, EditAddError>, FakeJournal) {
        let journal = FakeJournal::default();
        let result = add_task_in_editor(
            &journal,
            &clock(),
            &FakeEditor::writing(text),
            Placement::End,
        );
        (result, journal)
    }

    #[test]
    fn a_filled_in_template_adds_the_task_with_every_field_as_written() {
        let (result, journal) = add_written(
            "\
# ignored
Title:   Make it so
Kind: human
Links:
- github:owner/repo#7
- https://example.com/page
Body:
First paragraph.

    indented: Title: not a header
Criteria:
- it works
-   and it is fast
",
        );
        let Ok(Edited::Added(task)) = result else {
            panic!("not added: {result:?}");
        };
        assert_eq!(task.title, "Make it so");
        assert_eq!(task.kind, TaskKind::Human);
        assert_eq!(
            task.links,
            ["github:owner/repo#7", "https://example.com/page"]
        );
        assert_eq!(
            task.body,
            "First paragraph.\n\n    indented: Title: not a header"
        );
        assert_eq!(task.criteria, ["it works", "and it is fast"]);
        assert_eq!(list_tasks(&journal), Ok(vec![task]));
    }

    #[test]
    fn the_editor_is_opened_on_a_template_with_every_section() {
        let editor = FakeEditor::writing("");
        let journal = FakeJournal::default();
        add_task_in_editor(&journal, &clock(), &editor, Placement::End).unwrap();
        let opened = editor.opened_on.borrow().clone().unwrap();
        for header in ["Title:", "Kind: agent", "Links:", "Body:", "Criteria:"] {
            assert!(
                opened.lines().any(|line| line == header),
                "{header}: {opened}"
            );
        }
    }

    #[test]
    fn the_title_may_be_on_the_line_after_its_header() {
        let (result, _) = add_written("Title:\n  Later line\nCriteria:\n- c\n");
        let Ok(Edited::Added(task)) = result else {
            panic!("not added: {result:?}");
        };
        assert_eq!(task.title, "Later line");
        assert_eq!(task.kind, TaskKind::Agent);
    }

    #[test]
    fn a_template_left_unchanged_adds_nothing_even_with_a_final_newline_added_or_removed() {
        for text in [
            TEMPLATE.to_owned(),
            format!("{TEMPLATE}\n\n"),
            TEMPLATE.trim_end().to_owned(),
            TEMPLATE.replace('\n', "\r\n"),
            TEMPLATE.replace("\n-\n", "\n- \n"),
        ] {
            let (result, journal) = add_written(&text);
            assert_eq!(result, Ok(Edited::Unchanged), "{text:?}");
            assert_eq!(list_tasks(&journal), Ok(vec![]));
        }
    }

    #[test]
    fn an_emptied_template_adds_nothing() {
        for text in ["", "\n", "  \n\t\n"] {
            let (result, journal) = add_written(text);
            assert_eq!(result, Ok(Edited::Emptied), "{text:?}");
            assert_eq!(list_tasks(&journal), Ok(vec![]));
        }
    }

    #[test]
    fn a_template_with_a_kind_or_link_filled_in_but_nothing_else_is_invalid_not_unchanged() {
        let (result, _) = add_written(&TEMPLATE.replace("Kind: agent", "Kind: human"));
        assert_eq!(
            result,
            Err(EditAddError::Invalid(vec![
                TemplateProblem::Task(AddError::EmptyTitle),
                TemplateProblem::Task(AddError::NoCriteria),
            ]))
        );
    }

    #[test]
    fn every_problem_is_reported_at_once_and_nothing_is_added() {
        let (result, journal) = add_written(
            "Title:\nKind: robot\nLinks:\n- not a link\n- https://ok.example\n- github:x\nBody:\nCriteria:\n",
        );
        assert_eq!(
            result,
            Err(EditAddError::Invalid(vec![
                TemplateProblem::Kind("unknown kind \"robot\": expected agent or human".to_owned()),
                TemplateProblem::Task(AddError::EmptyTitle),
                TemplateProblem::Task(AddError::NoCriteria),
                TemplateProblem::Task(AddError::MalformedLink("not a link".to_owned())),
                TemplateProblem::Task(AddError::MalformedLink("github:x".to_owned())),
            ]))
        );
        assert_eq!(list_tasks(&journal), Ok(vec![]));
    }

    #[test]
    fn a_section_that_takes_one_line_with_more_is_a_problem() {
        let (result, _) = add_written("Title: a\nb\nKind: agent\nhuman\nCriteria:\n- c\n");
        assert_eq!(
            result,
            Err(EditAddError::Invalid(vec![
                TemplateProblem::ExtraLines("Title"),
                TemplateProblem::ExtraLines("Kind"),
            ]))
        );
    }

    #[test]
    fn the_problems_are_listed_one_to_a_line_in_the_message() {
        let (result, _) = add_written("Title:\nCriteria:\n");
        assert_eq!(
            result.unwrap_err().to_string(),
            "the task is not valid, so nothing was added\n  \
             - the title is empty: a task needs a title\n  \
             - a task needs at least one acceptance criterion"
        );
    }

    #[test]
    fn an_editor_that_fails_adds_nothing() {
        let journal = FakeJournal::default();
        let editor = FakeEditor::returning(Err(EditorError::new("boom")));
        let result = add_task_in_editor(&journal, &clock(), &editor, Placement::End);
        assert_eq!(result, Err(EditAddError::Editor(EditorError::new("boom"))));
        assert_eq!(list_tasks(&journal), Ok(vec![]));
    }

    #[test]
    fn the_task_goes_where_it_was_told() {
        let journal = FakeJournal::default();
        for title in ["a", "b"] {
            add_task(&journal, &clock(), &draft(title), Placement::End).unwrap();
        }
        let editor = FakeEditor::writing("Title: new\nCriteria:\n- c\n");
        add_task_in_editor(&journal, &clock(), &editor, Placement::Before(TaskId(2))).unwrap();
        add_task_in_editor(&journal, &clock(), &editor, Placement::After(TaskId(2))).unwrap();
        let titles: Vec<_> = list_tasks(&journal)
            .unwrap()
            .into_iter()
            .map(|task| task.title)
            .collect();
        assert_eq!(titles, ["a", "new", "b", "new"]);
    }

    #[test]
    fn a_place_that_does_not_exist_is_refused_before_the_editor_opens() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        journal.tasks.borrow_mut()[0].status = TaskStatus::Cancelled;
        add_task(&journal, &clock(), &draft("b"), Placement::End).unwrap();
        for (placement, error) in [
            (
                Placement::Before(TaskId(9)),
                AddError::UnknownTask(TaskId(9)),
            ),
            (
                Placement::After(TaskId(1)),
                AddError::CancelledTask(TaskId(1)),
            ),
        ] {
            let editor = FakeEditor::writing("Title: t\nCriteria:\n- c\n");
            let result = add_task_in_editor(&journal, &clock(), &editor, placement);
            assert_eq!(result, Err(EditAddError::Add(error)));
            assert_eq!(*editor.opened_on.borrow(), None);
        }
        assert_eq!(list_all_tasks(&journal).unwrap().len(), 2);
    }

    #[test]
    fn a_journal_that_cannot_be_used_adds_nothing() {
        let failure = JournalError::new("disk on fire");
        let journal = FakeJournal::failing(failure.clone());
        let editor = FakeEditor::writing("Title: t\nCriteria:\n- c\n");
        let result = add_task_in_editor(&journal, &clock(), &editor, Placement::End);
        assert_eq!(result, Err(EditAddError::Add(AddError::Journal(failure))));
    }
}

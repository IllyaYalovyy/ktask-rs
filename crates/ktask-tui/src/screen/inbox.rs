//! The input inbox: pending questions awaiting a human answer.
//!
//! A task that needs a human decision raises a [`DecisionRequest`] and pauses
//! for input (VISION.md §3 invariant 8). [`Inbox`] folds the journal into the
//! questions still waiting: a question is pending from the `DecisionRaised`
//! that paused its task until anything takes the task out of that pause, an
//! answer (`DecisionResolved`) usually, or a cancellation. It follows the
//! task's state through the core's own transition table rather than guessing
//! which events end a pause, so a question is listed exactly while
//! `ktask-rs resolve` would accept an answer for it.
//!
//! The screen shows the selected question with its context (the options and
//! their trade-offs), the impact of the decision and the agent's recommended
//! response. The answer is typed in place: `a` starts it, `r` starts it from
//! the recommended response, `Enter` sends it and `Esc` abandons it. While it
//! is typed every key is a letter of it, so `q` and the digits do not quit or
//! change screens; only `Ctrl-C` still leaves.
//!
//! Sending is an [`Action::Resolve`] in [`App::outbox`], like every other
//! action (see `docs/adr/0017-*.md`), because [`update`](crate::update) does no
//! I/O. What carries it out is [`resolve`], which is the same recording
//! `ktask-rs resolve` performs ([`ktask_core::resolve_decision`]), so the ADR
//! written from here is byte-identical to the command's. The question stays
//! listed until the journal says it is answered; the screen shows the
//! journal's state and never its own guess of it.
//!
//! Everything a question says came from an agent, so it is sanitized before
//! it is stored and wrapped at the edge of the pane when drawn.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::LayoutPlan;
use crate::sanitize::sanitize;
use crate::text::{display_width, truncate_to_width};
use crate::types::{Action, Screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{
    DecisionRequest, Event, EventKind, PauseReason, Project, TaskId, TaskState, apply,
    raised_request, resolve_decision,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use std::collections::BTreeMap;
use std::path::PathBuf;
use time::Date;
use unicode_segmentation::UnicodeSegmentation;

/// The longest text kept from any one field of a question.
const MAX_TEXT_CHARS: usize = 8_192;

/// The most options kept from a question.
const MAX_OPTIONS: usize = 64;

/// The most characters an answer can hold.
const MAX_ANSWER_CHARS: usize = 4_096;

/// What the body shows when no question is waiting.
const EMPTY: &str = "No decisions are waiting.";

/// The most pending questions listed above the detail.
const LIST_ROWS: usize = 3;

/// The fewest body rows that leave room for the list of questions.
const LIST_MIN_HEIGHT: u16 = 9;

/// The fewest body rows that leave room for a heading.
const HEADING_MIN_HEIGHT: u16 = 3;

/// The fewest body rows that leave room for the recommended response.
const RECOMMENDED_MIN_HEIGHT: u16 = 4;

/// The marker column that points at the selected question.
const MARKER: &str = "> ";

/// The marker column of a question that is not selected.
const NO_MARKER: &str = "  ";

/// What separates the parts of the heading.
const SEPARATOR: &str = " · ";

/// What separates the entries of the key bar.
const BAR_GAP: &str = "  ";

/// What precedes the answer being typed.
const ANSWER_PROMPT: &str = "Answer: ";

/// An answer being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draft {
    task: TaskId,
    text: String,
}

/// The questions waiting for an answer, and the answer being typed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inbox {
    /// Where each task stands, as far as the questions need to know.
    states: BTreeMap<TaskId, TaskState>,
    /// The question of every task now paused for input, by task.
    pending: BTreeMap<TaskId, DecisionRequest>,
    /// The task the cursor is on; the first pending one when it is not
    /// pending (or is `None`).
    cursor: Option<TaskId>,
    draft: Option<Draft>,
}

impl Inbox {
    /// How many questions are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether no question is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The tasks with a question waiting, in queue order.
    pub fn tasks(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.pending.keys().copied()
    }

    /// The question of `task`, if it is waiting.
    #[must_use]
    pub fn question(&self, task: TaskId) -> Option<&DecisionRequest> {
        self.pending.get(&task)
    }

    /// The task the cursor is on: the one selected, or else the first
    /// waiting.
    #[must_use]
    pub fn selected(&self) -> Option<TaskId> {
        self.cursor
            .filter(|task| self.pending.contains_key(task))
            .or_else(|| self.pending.keys().next().copied())
    }

    /// The answer being typed and the task it is for.
    #[must_use]
    pub fn draft(&self) -> Option<(TaskId, &str)> {
        self.draft
            .as_ref()
            .map(|draft| (draft.task, draft.text.as_str()))
    }

    /// Whether an answer is being typed, in which case keys are its letters.
    #[must_use]
    pub fn is_typing(&self) -> bool {
        self.draft.is_some()
    }
}

/// `text` sanitized, trimmed and no longer than [`MAX_TEXT_CHARS`].
fn tidy(text: &str) -> String {
    let clean = sanitize(text);
    let clean = clean.trim();
    match clean.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", clean.get(..end).unwrap_or_default()),
        None => clean.to_owned(),
    }
}

/// `request` as the screen keeps it: every field tidied, at most
/// [`MAX_OPTIONS`] options, and a recommendation only if it says anything.
fn tidy_request(request: &DecisionRequest) -> DecisionRequest {
    DecisionRequest {
        question: tidy(&request.question),
        options: request
            .options
            .iter()
            .take(MAX_OPTIONS)
            .map(|option| tidy(option))
            .collect(),
        tradeoffs: tidy(&request.tradeoffs),
        impact: tidy(&request.impact),
        recommended: request
            .recommended
            .as_deref()
            .map(tidy)
            .filter(|recommended| !recommended.is_empty()),
    }
}

/// Folds one journal event into the inbox.
///
/// The task's state is advanced by the core's transition table; an event the
/// table refuses leaves it where it was. The question is kept while the state
/// is a pause for input and dropped as soon as it is not. An answer being typed
/// for a question that is no longer waiting is dropped with a notice, since
/// sending it could only be refused.
pub fn fold(app: &mut App, event: &Event) {
    let Some(task) = event.task_id else { return };
    if matches!(event.kind, EventKind::AgentOutput { .. }) {
        return;
    }
    let inbox = &mut app.inbox;
    let state = inbox.states.entry(task).or_insert(TaskState::Queued);
    let Ok(next) = apply(state, &event.kind) else {
        return;
    };
    *state = next;
    let waiting = matches!(
        state,
        TaskState::Paused {
            reason: PauseReason::Input,
            ..
        }
    );
    match &event.kind {
        EventKind::DecisionRaised { request } if waiting => {
            inbox.pending.insert(task, tidy_request(request));
        }
        _ if !waiting => {
            inbox.pending.remove(&task);
        }
        _ => {}
    }
    let orphaned = inbox
        .draft
        .as_ref()
        .filter(|draft| !inbox.pending.contains_key(&draft.task))
        .map(|draft| draft.task);
    if let Some(orphaned) = orphaned {
        inbox.draft = None;
        app.notice = Some(format!(
            "answer: task {orphaned} is no longer waiting for a decision; the answer was dropped"
        ));
    }
}

/// Whether the inbox has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::InputInbox && app.overlay.is_none()
}

/// Takes `key` as a letter of the answer being typed, if one is. Returns
/// whether it did, in which case no other part of the interface sees the key:
/// `q`, digits and `?` are letters of the answer, not commands.
///
/// `Enter` sends the answer, `Esc` abandons it and `Backspace` removes a
/// letter. Keys held with `Ctrl` or `Alt` are not letters and change nothing.
/// Any key press here first clears the notice the last one left.
pub fn capture(app: &mut App, key: &KeyEvent) -> bool {
    if !has_focus(app) || !app.inbox.is_typing() {
        return false;
    }
    app.notice = None;
    match key.code {
        KeyCode::Esc => app.inbox.draft = None,
        KeyCode::Enter => submit(app),
        KeyCode::Backspace => {
            if let Some(draft) = app.inbox.draft.as_mut() {
                draft.text.pop();
            }
        }
        KeyCode::Char(c)
            if !c.is_control()
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(draft) = app.inbox.draft.as_mut()
                && draft.text.chars().count() < MAX_ANSWER_CHARS
            {
                draft.text.push(c);
            }
        }
        _ => {}
    }
    true
}

/// Sends the answer being typed: an [`Action::Resolve`] in the outbox. A blank
/// answer is refused and stays open for more.
fn submit(app: &mut App) {
    let Some(draft) = app.inbox.draft.as_ref() else {
        return;
    };
    let task = draft.task;
    let note = draft.text.trim().to_owned();
    if note.is_empty() {
        app.notice = Some("resolve: the answer is empty".to_owned());
        return;
    }
    app.inbox.draft = None;
    app.outbox.push(Action::Resolve { task, note });
    app.notice = Some(format!("resolve: answer to task {task} sent"));
}

/// Starts an answer to the selected question, from its recommended response
/// when `recommended`, otherwise empty.
fn start_answer(app: &mut App, recommended: bool) {
    let Some(task) = app.inbox.selected() else {
        app.notice = Some("answer: no decision is waiting".to_owned());
        return;
    };
    let text = if recommended {
        let suggestion = app
            .inbox
            .question(task)
            .and_then(|question| question.recommended.as_deref());
        let Some(suggestion) = suggestion else {
            app.notice = Some(format!("answer: task {task} has no recommended response"));
            return;
        };
        suggestion.chars().take(MAX_ANSWER_CHARS).collect()
    } else {
        String::new()
    };
    app.inbox.draft = Some(Draft { task, text });
}

/// Handles the inbox's keys: `a` or `Enter` to answer, `r` to answer from the
/// recommended response, and `j`, `k`, the arrows, `g` and `G` to move between
/// questions. Does nothing on other screens, under an overlay, or while an
/// answer is being typed (see [`capture`]). Any key press here first clears the
/// notice the last one left.
///
/// The selection stops at the ends rather than wrapping.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) || app.inbox.is_typing() {
        return;
    }
    app.notice = None;
    if key.modifiers == KeyModifiers::NONE {
        match key.code {
            KeyCode::Char('a') | KeyCode::Enter => return start_answer(app, false),
            KeyCode::Char('r') => return start_answer(app, true),
            _ => {}
        }
    }
    let tasks: Vec<TaskId> = app.inbox.tasks().collect();
    let (Some(current), Some(last)) = (
        app.inbox
            .selected()
            .and_then(|task| tasks.iter().position(|candidate| *candidate == task)),
        tasks.len().checked_sub(1),
    ) else {
        return;
    };
    let next = match lookup(app.screen, key).map(|binding| binding.action) {
        Some(KeyAction::MoveDown) => (current + 1).min(last),
        Some(KeyAction::MoveUp) => current.saturating_sub(1),
        Some(KeyAction::First) => 0,
        Some(KeyAction::Last) => last,
        _ => return,
    };
    app.inbox.cursor = tasks.get(next).copied();
}

/// The longest suffix of `text` made of whole grapheme clusters that fits in
/// `width` columns.
fn tail_to_width(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut kept: Vec<&str> = Vec::new();
    for cluster in text.graphemes(true).rev() {
        used += display_width(cluster);
        if used > width {
            break;
        }
        kept.push(cluster);
    }
    kept.into_iter().rev().collect()
}

/// The first line of `text` that says anything.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// The heading: how many questions wait, which one is shown, and what the
/// keys do while an answer is being typed.
fn heading(inbox: &Inbox) -> String {
    let mut parts = vec![format!("Pending decisions: {}", inbox.len())];
    let position = inbox
        .selected()
        .and_then(|task| inbox.tasks().position(|candidate| candidate == task));
    if let (Some(position), true) = (position, inbox.len() > 1) {
        parts.push(format!("{} of {}", position + 1, inbox.len()));
    }
    if inbox.is_typing() {
        parts.push("Enter sends the answer, Esc abandons it".to_owned());
    }
    parts.join(SEPARATOR)
}

/// The line of keys shown under the question while no answer is typed.
fn key_bar(inbox: &Inbox, question: &DecisionRequest) -> String {
    let mut keys = vec!["a answer"];
    if question.recommended.is_some() {
        keys.push("r use recommended");
    }
    if inbox.len() > 1 {
        keys.push("j/k select");
    }
    keys.join(BAR_GAP)
}

/// The answer line: the prompt, the end of what is typed and a cursor.
fn answer_line(text: &str, width: usize) -> Line<'static> {
    let room = width.saturating_sub(ANSWER_PROMPT.len() + 1);
    Line::from(vec![
        Span::styled(ANSWER_PROMPT, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(tail_to_width(text, room)),
        Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
    ])
}

/// A bold label.
fn label(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), Style::new().add_modifier(Modifier::BOLD))
}

/// The lines of the selected question: what is asked, the options, their
/// trade-offs and the impact.
fn detail_lines(task: TaskId, question: &DecisionRequest) -> Vec<Line<'static>> {
    let mut lines = vec![label(&format!("Task {task} asks"))];
    lines.extend(
        question
            .question
            .lines()
            .map(|line| Line::from(line.to_owned())),
    );
    if !question.options.is_empty() {
        lines.push(Line::default());
        lines.push(label("Options"));
        lines.extend(
            question
                .options
                .iter()
                .map(|option| Line::from(format!("- {option}"))),
        );
    }
    for (name, text) in [
        ("Trade-offs", &question.tradeoffs),
        ("Impact", &question.impact),
    ] {
        if !text.is_empty() {
            lines.push(Line::default());
            lines.push(label(name));
            lines.extend(text.lines().map(|line| Line::from(line.to_owned())));
        }
    }
    lines
}

/// Draws the pending questions, at most [`LIST_ROWS`], scrolled so the
/// selected one is in view.
fn render_list(inbox: &Inbox, area: Rect, frame: &mut Frame<'_>) {
    let width = usize::from(area.width);
    let selected = inbox.selected();
    let rows = usize::from(area.height);
    let at = selected
        .and_then(|task| inbox.tasks().position(|candidate| candidate == task))
        .unwrap_or(0);
    let offset = (at + 1).saturating_sub(rows);
    let lines: Vec<Line<'static>> = inbox
        .tasks()
        .skip(offset)
        .take(rows)
        .map(|task| {
            let marker = if Some(task) == selected {
                MARKER
            } else {
                NO_MARKER
            };
            let asked = inbox
                .question(task)
                .map_or("", |question| first_line(&question.question));
            let text = truncate_to_width(&format!("{marker}Task {task}  {asked}"), width);
            if Some(task) == selected {
                Line::styled(text, Style::new().add_modifier(Modifier::BOLD))
            } else {
                Line::from(text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Draws the inbox into the body of `plan`: a heading, the pending questions
/// when there are several, the selected question in full, its recommended
/// response and, last, the keys or the answer being typed. Each appears where
/// the body is tall enough for it, and the question gives up rows before the
/// recommendation or the answer do.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let inbox = &app.inbox;
    let question = inbox
        .selected()
        .and_then(|task| inbox.question(task).map(|question| (task, question)));
    let recommended = question
        .and_then(|(_, question)| question.recommended.as_deref())
        .filter(|_| body.height >= RECOMMENDED_MIN_HEIGHT);
    let [heading_area, main_area, recommended_area, bar_area] = Layout::vertical([
        Constraint::Length(u16::from(body.height >= HEADING_MIN_HEIGHT)),
        Constraint::Fill(1),
        Constraint::Length(u16::from(recommended.is_some())),
        Constraint::Length(1),
    ])
    .areas(body);

    let notice_style = Style::new().fg(Color::Yellow);
    let (heading_text, heading_style) = match &app.notice {
        Some(notice) => (notice.clone(), notice_style),
        None => (heading(inbox), Style::new().add_modifier(Modifier::BOLD)),
    };
    frame.render_widget(
        Paragraph::new(truncate_to_width(&heading_text, width)).style(heading_style),
        heading_area,
    );

    let list_rows = if inbox.len() > 1 && main_area.height >= LIST_MIN_HEIGHT {
        u16::try_from(inbox.len().min(LIST_ROWS)).unwrap_or(0)
    } else {
        0
    };
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Length(list_rows), Constraint::Fill(1)]).areas(main_area);
    match question {
        None => frame.render_widget(
            Paragraph::new(truncate_to_width(EMPTY, width))
                .style(Style::new().add_modifier(Modifier::DIM)),
            detail_area,
        ),
        Some((task, question)) => {
            render_list(inbox, list_area, frame);
            frame.render_widget(
                Paragraph::new(detail_lines(task, question)).wrap(Wrap { trim: false }),
                detail_area,
            );
        }
    }
    if let Some(recommended) = recommended {
        let text = truncate_to_width(&format!("Recommended: {}", first_line(recommended)), width);
        frame.render_widget(
            Paragraph::new(text).style(Style::new().fg(Color::Green)),
            recommended_area,
        );
    }

    let last_row = if let Some((_, text)) = inbox.draft() {
        answer_line(text, width)
    } else if let (Some(notice), 0) = (&app.notice, heading_area.height) {
        // No heading row to carry the notice, so the bar's row does.
        Line::styled(truncate_to_width(notice, width), notice_style)
    } else if let Some((_, question)) = question {
        Line::from(truncate_to_width(&key_bar(inbox, question), width))
    } else {
        Line::default()
    };
    frame.render_widget(Paragraph::new(last_row), bar_area);
}

/// Carries out the [`Action::Resolve`] for `task`: records `answer` exactly as
/// `ktask-rs resolve --note` does and returns the ADR's path relative to the
/// repository root.
///
/// # Errors
///
/// The reason, in the words the command uses, when the answer is empty, the
/// task asked no question, is not waiting for input, or the record could not
/// be written. Only a failure to write the ADR leaves the answer journaled.
pub fn resolve(
    project: &Project,
    task: TaskId,
    answer: &str,
    today: Date,
) -> Result<PathBuf, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Err("resolve: the answer is empty".to_owned());
    }
    let request = match raised_request(project, task) {
        Ok(Some(request)) => request,
        Ok(None) => return Err(format!("resolve: task {task} asked no question to answer")),
        Err(err) => return Err(format!("resolve: could not read the journal: {err}")),
    };
    resolve_decision(project, task, &request, answer, today)
        .map_err(|err| format!("resolve: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::update;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::Overlay;
    use ktask_core::{AttemptId, EventSeq, Journal, Task, TaskStatus};
    use time::{Month, OffsetDateTime};

    fn today() -> Date {
        Date::from_calendar_date(2026, Month::September, 23).expect("a real date")
    }

    fn request() -> DecisionRequest {
        DecisionRequest {
            question: "Postgres or SQLite for the journal?".to_string(),
            options: vec!["Postgres".to_string(), "SQLite".to_string()],
            tradeoffs: "Postgres scales; SQLite is one file.".to_string(),
            impact: "Journal durability and operational overhead.".to_string(),
            recommended: Some("SQLite".to_string()),
        }
    }

    fn event(task: u32, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(task)),
            kind,
        }
    }

    /// The events that take `task` from the queue to waiting on `request`.
    fn asks(task: u32, request: &DecisionRequest) -> Vec<Event> {
        vec![
            event(task, EventKind::PreflightStarted),
            event(
                task,
                EventKind::PreflightPassed {
                    base_sha: "abc".into(),
                },
            ),
            event(
                task,
                EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".into(),
                    pid: 1,
                    base_sha: "abc".into(),
                },
            ),
            event(
                task,
                EventKind::DecisionRaised {
                    request: request.clone(),
                },
            ),
        ]
    }

    fn feed(app: App, events: Vec<Event>) -> App {
        events
            .into_iter()
            .fold(app, |app, event| update(app, AppEvent::Core(event)))
    }

    fn app_at(size: (u16, u16), events: Vec<Event>) -> App {
        feed(
            App {
                screen: Screen::InputInbox,
                ..App::new(size)
            },
            events,
        )
    }

    fn waiting(size: (u16, u16)) -> App {
        app_at(size, asks(1, &request()))
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn typed(app: App, text: &str) -> App {
        text.chars()
            .fold(app, |app, c| press(app, KeyCode::Char(c)))
    }

    fn rows(harness: &Harness) -> Vec<String> {
        harness.text().lines().map(str::to_owned).collect()
    }

    fn shows(harness: &Harness, needle: &str) -> bool {
        rows(harness).iter().any(|row| row.contains(needle))
    }

    // ---- what the inbox folds out of the journal ----

    #[test]
    fn inbox_a_raised_question_is_pending_with_its_whole_request() {
        let app = waiting((80, 24));
        assert_eq!(app.inbox.len(), 1);
        assert!(!app.inbox.is_empty());
        assert_eq!(app.inbox.tasks().collect::<Vec<_>>(), [TaskId::new(1)]);
        assert_eq!(app.inbox.question(TaskId::new(1)), Some(&request()));
        assert_eq!(app.inbox.selected(), Some(TaskId::new(1)));
    }

    #[test]
    fn inbox_starts_empty() {
        let app = App::new((80, 24));
        assert!(app.inbox.is_empty());
        assert_eq!(app.inbox.len(), 0);
        assert_eq!(app.inbox.selected(), None);
        assert_eq!(app.inbox.draft(), None);
        assert!(!app.inbox.is_typing());
    }

    #[test]
    fn inbox_a_resolution_removes_the_question() {
        let app = feed(
            waiting((80, 24)),
            vec![event(
                1,
                EventKind::DecisionResolved {
                    adr_path: "docs/adr/0001-x.md".into(),
                    answer: "SQLite".into(),
                },
            )],
        );
        assert!(app.inbox.is_empty());
    }

    #[test]
    fn inbox_a_cancellation_removes_the_question() {
        let app = feed(
            waiting((80, 24)),
            vec![event(
                1,
                EventKind::TaskCancelled {
                    reason: "no longer needed".into(),
                },
            )],
        );
        assert!(app.inbox.is_empty());
    }

    #[test]
    fn inbox_a_resume_removes_the_question() {
        let app = feed(waiting((80, 24)), vec![event(1, EventKind::Resumed)]);
        assert!(app.inbox.is_empty());
    }

    #[test]
    fn inbox_a_question_raised_again_after_an_answer_is_pending_again() {
        let mut second = request();
        second.question = "Which port?".into();
        let mut events = vec![event(
            1,
            EventKind::DecisionResolved {
                adr_path: "docs/adr/0001-x.md".into(),
                answer: "SQLite".into(),
            },
        )];
        events.extend(asks(1, &second));
        let app = feed(waiting((80, 24)), events);
        assert_eq!(app.inbox.question(TaskId::new(1)), Some(&second));
    }

    #[test]
    fn inbox_output_does_not_change_what_is_pending() {
        let before = waiting((80, 24));
        let after = feed(
            before.clone(),
            vec![event(
                1,
                EventKind::AgentOutput {
                    attempt: AttemptId::new(1),
                    stream: ktask_core::Stream::Stdout,
                    text: "thinking".into(),
                },
            )],
        );
        assert_eq!(after.inbox, before.inbox);
    }

    #[test]
    fn inbox_lists_questions_in_queue_order_whatever_order_they_were_asked() {
        let mut other = request();
        other.question = "Which port?".into();
        let mut events = asks(3, &other);
        events.extend(asks(2, &request()));
        let app = app_at((80, 24), events);
        assert_eq!(
            app.inbox.tasks().collect::<Vec<_>>(),
            [TaskId::new(2), TaskId::new(3)]
        );
        assert_eq!(app.inbox.selected(), Some(TaskId::new(2)));
    }

    #[test]
    fn inbox_a_pause_for_input_with_no_question_is_not_listed() {
        let events = vec![
            event(1, EventKind::PreflightStarted),
            event(
                1,
                EventKind::PreflightPassed {
                    base_sha: "abc".into(),
                },
            ),
            event(
                1,
                EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".into(),
                    pid: 1,
                    base_sha: "abc".into(),
                },
            ),
            event(
                1,
                EventKind::Paused {
                    reason: PauseReason::Input,
                },
            ),
        ];
        assert!(app_at((80, 24), events).inbox.is_empty());
    }

    #[test]
    fn inbox_a_pause_for_something_else_is_not_listed() {
        let mut events = asks(1, &request());
        events.pop();
        events.push(event(
            1,
            EventKind::Paused {
                reason: PauseReason::Blocked,
            },
        ));
        assert!(app_at((80, 24), events).inbox.is_empty());
    }

    #[test]
    fn inbox_an_event_with_no_task_changes_nothing() {
        let before = waiting((80, 24));
        let mut stray = event(1, EventKind::Resumed);
        stray.task_id = None;
        assert_eq!(feed(before.clone(), vec![stray]).inbox, before.inbox);
    }

    #[test]
    fn inbox_a_question_is_sanitized_and_bounded_before_it_is_kept() {
        let mut hostile = request();
        hostile.question = "Use \x1b[31mred\x1b[0m?\r\x07".into();
        hostile.options = vec!["a\x1b]0;title\x07b".into(); 200];
        hostile.tradeoffs = "x".repeat(MAX_TEXT_CHARS * 2);
        hostile.recommended = Some("\x1b[2Jgo".into());
        let app = app_at((80, 24), asks(1, &hostile));
        let kept = app.inbox.question(TaskId::new(1)).expect("kept");
        assert!(!kept.question.contains('\x1b'), "{:?}", kept.question);
        assert!(kept.question.contains("Use red?"), "{:?}", kept.question);
        assert_eq!(kept.options.len(), MAX_OPTIONS);
        assert_eq!(kept.options[0], "ab");
        assert_eq!(kept.tradeoffs.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(kept.tradeoffs.ends_with('…'));
        assert_eq!(kept.recommended.as_deref(), Some("go"));
    }

    // ---- selecting a question ----

    fn two_questions() -> App {
        let mut other = request();
        other.question = "Which port?".into();
        let mut events = asks(1, &request());
        events.extend(asks(2, &other));
        app_at((80, 24), events)
    }

    #[test]
    fn inbox_j_and_k_move_between_questions_and_stop_at_the_ends() {
        let app = two_questions();
        assert_eq!(app.inbox.selected(), Some(TaskId::new(1)));
        let app = press(app, KeyCode::Char('j'));
        assert_eq!(app.inbox.selected(), Some(TaskId::new(2)));
        let app = press(app, KeyCode::Down);
        assert_eq!(app.inbox.selected(), Some(TaskId::new(2)));
        let app = press(app, KeyCode::Char('k'));
        assert_eq!(app.inbox.selected(), Some(TaskId::new(1)));
        let app = press(app, KeyCode::Up);
        assert_eq!(app.inbox.selected(), Some(TaskId::new(1)));
    }

    #[test]
    fn inbox_g_and_capital_g_select_the_first_and_the_last_question() {
        let app = two_questions();
        let app = press(app, KeyCode::Char('G'));
        assert_eq!(app.inbox.selected(), Some(TaskId::new(2)));
        let app = press(app, KeyCode::Char('g'));
        assert_eq!(app.inbox.selected(), Some(TaskId::new(1)));
    }

    #[test]
    fn inbox_the_selection_falls_back_to_the_first_when_its_question_is_answered() {
        let app = press(two_questions(), KeyCode::Char('j'));
        let app = feed(app, vec![event(2, EventKind::Resumed)]);
        assert_eq!(app.inbox.selected(), Some(TaskId::new(1)));
    }

    #[test]
    fn inbox_movement_keys_do_nothing_when_no_question_waits() {
        let app = app_at((80, 24), Vec::new());
        for code in [KeyCode::Char('j'), KeyCode::Char('G'), KeyCode::Up] {
            assert_eq!(press(app.clone(), code).inbox, app.inbox);
        }
    }

    #[test]
    fn inbox_keys_do_nothing_on_other_screens_or_under_an_overlay() {
        let mut elsewhere = two_questions();
        elsewhere.screen = Screen::Git;
        for code in [KeyCode::Char('j'), KeyCode::Char('a'), KeyCode::Char('r')] {
            let after = press(elsewhere.clone(), code);
            assert_eq!(after.inbox, elsewhere.inbox, "{code:?}");
            assert_eq!(after.notice, None);
        }
        let mut covered = two_questions();
        covered.overlay = Some(Overlay::KeyMap);
        for code in [KeyCode::Char('j'), KeyCode::Char('a'), KeyCode::Char('r')] {
            assert_eq!(
                press(covered.clone(), code).inbox,
                covered.inbox,
                "{code:?}"
            );
        }
    }

    // ---- answering in place ----

    #[test]
    fn inbox_a_starts_an_empty_answer_for_the_selected_question() {
        let app = press(
            press(two_questions(), KeyCode::Char('j')),
            KeyCode::Char('a'),
        );
        assert_eq!(app.inbox.draft(), Some((TaskId::new(2), "")));
        assert!(app.inbox.is_typing());
    }

    #[test]
    fn inbox_enter_also_starts_an_answer() {
        let app = press(waiting((80, 24)), KeyCode::Enter);
        assert_eq!(app.inbox.draft(), Some((TaskId::new(1), "")));
    }

    #[test]
    fn inbox_r_starts_the_answer_from_the_recommended_response() {
        let app = press(waiting((80, 24)), KeyCode::Char('r'));
        assert_eq!(app.inbox.draft(), Some((TaskId::new(1), "SQLite")));
    }

    #[test]
    fn inbox_r_says_so_when_the_agent_recommended_nothing() {
        let mut bare = request();
        bare.recommended = None;
        let app = press(app_at((80, 24), asks(1, &bare)), KeyCode::Char('r'));
        assert_eq!(app.inbox.draft(), None);
        let notice = app.notice.expect("a notice");
        assert!(notice.contains("no recommended response"), "{notice}");
        assert!(notice.contains("task 1"), "{notice}");
    }

    #[test]
    fn inbox_answering_with_nothing_waiting_says_so() {
        let app = press(app_at((80, 24), Vec::new()), KeyCode::Char('a'));
        assert_eq!(app.inbox.draft(), None);
        let notice = app.notice.expect("a notice");
        assert!(notice.contains("no decision is waiting"), "{notice}");
    }

    #[test]
    fn inbox_typed_keys_are_letters_of_the_answer_and_backspace_removes_one() {
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "Use q 6 ? é");
        assert_eq!(app.inbox.draft(), Some((TaskId::new(1), "Use q 6 ? é")));
        assert_eq!(app.screen, Screen::InputInbox);
        assert_eq!(app.overlay, None);
        let app = press(press(app, KeyCode::Backspace), KeyCode::Backspace);
        assert_eq!(app.inbox.draft(), Some((TaskId::new(1), "Use q 6 ?")));
    }

    #[test]
    fn inbox_control_keys_and_tab_are_not_letters_of_the_answer() {
        let app = press(waiting((80, 24)), KeyCode::Char('a'));
        let app = update(
            app,
            AppEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        );
        let app = press(press(app, KeyCode::Tab), KeyCode::F(1));
        assert_eq!(app.inbox.draft(), Some((TaskId::new(1), "")));
        assert_eq!(app.screen, Screen::InputInbox);
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn inbox_an_answer_is_bounded() {
        let app = typed(
            press(waiting((80, 24)), KeyCode::Char('a')),
            &"a".repeat(MAX_ANSWER_CHARS + 10),
        );
        let (_, text) = app.inbox.draft().expect("typing");
        assert_eq!(text.chars().count(), MAX_ANSWER_CHARS);
    }

    #[test]
    fn inbox_escape_abandons_the_answer_and_sends_nothing() {
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "half");
        let app = press(app, KeyCode::Esc);
        assert_eq!(app.inbox.draft(), None);
        assert!(app.outbox.is_empty());
        assert_eq!(app.screen, Screen::InputInbox);
        assert_eq!(app.inbox.len(), 1);
    }

    #[test]
    fn inbox_enter_sends_the_trimmed_answer_as_a_resolve_action() {
        let app = typed(
            press(waiting((80, 24)), KeyCode::Char('a')),
            "  Use SQLite.  ",
        );
        let mut app = press(app, KeyCode::Enter);
        assert_eq!(
            app.take_outbox(),
            [Action::Resolve {
                task: TaskId::new(1),
                note: "Use SQLite.".into(),
            }]
        );
        assert_eq!(app.inbox.draft(), None);
        let notice = app.notice.expect("a notice");
        assert!(notice.contains("task 1"), "{notice}");
    }

    #[test]
    fn inbox_the_recommended_response_can_be_edited_before_it_is_sent() {
        let app = press(waiting((80, 24)), KeyCode::Char('r'));
        let app = typed(press(app, KeyCode::Backspace), "e, please");
        let mut app = press(app, KeyCode::Enter);
        assert_eq!(
            app.take_outbox(),
            [Action::Resolve {
                task: TaskId::new(1),
                note: "SQLite, please".into(),
            }]
        );
    }

    #[test]
    fn inbox_an_answer_goes_to_the_question_it_was_started_on() {
        let app = press(two_questions(), KeyCode::Char('j'));
        let app = typed(press(app, KeyCode::Char('a')), "8080");
        let mut app = press(app, KeyCode::Enter);
        assert_eq!(
            app.take_outbox(),
            [Action::Resolve {
                task: TaskId::new(2),
                note: "8080".into(),
            }]
        );
    }

    #[test]
    fn inbox_a_blank_answer_is_refused_and_the_answer_stays_open() {
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "   ");
        let app = press(app, KeyCode::Enter);
        assert!(app.outbox.is_empty());
        assert!(app.inbox.is_typing());
        let notice = app.notice.expect("a notice");
        assert!(notice.contains("empty"), "{notice}");
    }

    #[test]
    fn inbox_a_key_after_a_notice_clears_it() {
        let app = press(app_at((80, 24), Vec::new()), KeyCode::Char('a'));
        assert!(app.notice.is_some());
        assert_eq!(press(app, KeyCode::Char('j')).notice, None);
    }

    #[test]
    fn inbox_a_notice_is_cleared_by_the_next_typed_key_too() {
        let app = press(press(waiting((80, 24)), KeyCode::Char('a')), KeyCode::Enter);
        assert!(app.notice.is_some());
        assert_eq!(press(app, KeyCode::Char('x')).notice, None);
    }

    #[test]
    fn inbox_an_answer_being_typed_is_dropped_when_its_question_is_answered_elsewhere() {
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "SQLite");
        let app = feed(app, vec![event(1, EventKind::Resumed)]);
        assert_eq!(app.inbox.draft(), None);
        let notice = app.notice.expect("a notice");
        assert!(notice.contains("task 1"), "{notice}");
        assert!(notice.contains("no longer waiting"), "{notice}");
    }

    #[test]
    fn inbox_an_answer_survives_other_tasks_events() {
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "SQLite");
        let app = feed(
            app,
            vec![event(
                2,
                EventKind::TaskQueued {
                    title: "Another".into(),
                },
            )],
        );
        assert_eq!(app.inbox.draft(), Some((TaskId::new(1), "SQLite")));
        assert_eq!(app.notice, None);
    }

    // ---- the screen ----

    #[test]
    fn inbox_shows_the_context_impact_and_recommended_response() {
        let harness = Harness::from_app(waiting((80, 24)));
        for expected in [
            "Input inbox",
            "Pending decisions: 1",
            "Task 1",
            "Postgres or SQLite for the journal?",
            "- Postgres",
            "- SQLite",
            "Trade-offs",
            "Postgres scales; SQLite is one file.",
            "Impact",
            "Journal durability and operational overhead.",
            "Recommended: SQLite",
        ] {
            assert!(
                shows(&harness, expected),
                "{expected:?} in\n{}",
                harness.text()
            );
        }
    }

    #[test]
    fn inbox_says_when_nothing_is_waiting() {
        let harness = Harness::from_app(app_at((80, 24), Vec::new()));
        assert!(shows(&harness, EMPTY), "{}", harness.text());
        assert!(!shows(&harness, "Recommended"));
    }

    #[test]
    fn inbox_without_a_recommendation_shows_none() {
        let mut bare = request();
        bare.recommended = None;
        let harness = Harness::from_app(app_at((80, 24), asks(1, &bare)));
        assert!(!shows(&harness, "Recommended"), "{}", harness.text());
        assert!(shows(&harness, "Impact"));
    }

    #[test]
    fn inbox_the_bar_names_the_keys_until_an_answer_is_started() {
        let harness = Harness::from_app(waiting((80, 24)));
        let keys = rows(&harness)
            .into_iter()
            .find(|row| row.contains("a answer"))
            .expect("the row of keys");
        assert!(keys.contains("r use recommended"), "{keys}");
        assert!(
            !keys.contains("j/k"),
            "one question needs no selecting: {keys}"
        );
    }

    #[test]
    fn inbox_the_answer_being_typed_is_shown_after_a_prompt() {
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "Use SQLite");
        let harness = Harness::from_app(app);
        assert!(shows(&harness, "Answer: Use SQLite"), "{}", harness.text());
        assert!(shows(&harness, "Enter sends"), "{}", harness.text());
        assert!(!shows(&harness, "a answer"), "{}", harness.text());
    }

    #[test]
    fn inbox_a_long_answer_shows_its_end_within_the_width() {
        let text = format!("{}END", "x".repeat(200));
        let app = typed(press(waiting((40, 24)), KeyCode::Char('a')), &text);
        let harness = Harness::from_app(app);
        let row = rows(&harness)
            .into_iter()
            .find(|row| row.contains("Answer:"))
            .expect("the answer row");
        assert!(row.contains("xxxEND"), "{row}");
        assert!(display_width(&row) <= 40);
    }

    #[test]
    fn inbox_a_wide_answer_is_cut_on_whole_characters() {
        let text = "决定".repeat(40);
        let app = typed(press(waiting((21, 24)), KeyCode::Char('a')), &text);
        let harness = Harness::from_app(app);
        let row = rows(&harness)
            .into_iter()
            .find(|row| row.contains("Answer:"))
            .expect("the answer row");
        // The test screen pads a wide character with a blank cell, so what is
        // left when the blanks go is the prompt and the characters that fit:
        // twelve columns are six of them.
        let shown: String = row.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(shown, "Answer:决定决定决定");
    }

    #[test]
    fn inbox_the_tail_of_an_answer_is_cut_on_whole_grapheme_clusters() {
        assert_eq!(tail_to_width("hello", 3), "llo");
        assert_eq!(tail_to_width("hello", 5), "hello");
        assert_eq!(tail_to_width("hello", 9), "hello");
        assert_eq!(tail_to_width("hello", 0), "");
        assert_eq!(tail_to_width("决定决定", 5), "决定");
        assert_eq!(tail_to_width("决定决定", 1), "");
        // A base letter and its combining mark are one cluster.
        assert_eq!(tail_to_width("xe\u{301}", 1), "e\u{301}");
    }

    #[test]
    fn inbox_a_notice_is_shown_and_an_empty_answer_says_why() {
        let app = press(
            typed(press(waiting((80, 24)), KeyCode::Char('a')), " "),
            KeyCode::Enter,
        );
        let harness = Harness::from_app(app);
        assert!(shows(&harness, "the answer is empty"), "{}", harness.text());
        assert!(shows(&harness, "Answer:"), "{}", harness.text());
    }

    #[test]
    fn inbox_lists_every_waiting_question_and_marks_the_selected_one() {
        let app = press(two_questions(), KeyCode::Char('j'));
        let harness = Harness::from_app(app);
        let marked = rows(&harness)
            .into_iter()
            .find(|row| row.starts_with("> "))
            .expect("a marked row");
        assert!(marked.contains("Task 2"), "{marked}");
        assert!(marked.contains("Which port"), "{marked}");
        let other = rows(&harness)
            .into_iter()
            .find(|row| row.starts_with("  ") && row.contains("Task 1"))
            .expect("the other question");
        assert!(other.contains("Postgres or SQLite"), "{other}");
        assert!(
            shows(&harness, "Pending decisions: 2 · 2 of 2"),
            "{}",
            harness.text()
        );
    }

    #[test]
    fn inbox_one_question_is_not_listed_above_its_own_detail() {
        let harness = Harness::from_app(waiting((80, 24)));
        assert!(rows(&harness).iter().all(|row| !row.starts_with("> ")));
    }

    #[test]
    fn inbox_long_lines_are_wrapped_not_cut() {
        let mut long = request();
        long.tradeoffs = "alpha ".repeat(30) + "omega";
        let harness = Harness::from_app(app_at((40, 24), asks(1, &long)));
        assert!(shows(&harness, "omega"), "{}", harness.text());
        assert!(rows(&harness).iter().all(|row| display_width(row) <= 40));
    }

    #[test]
    fn inbox_keeps_the_recommendation_and_the_bar_when_the_detail_is_taller_than_the_pane() {
        let mut long = request();
        long.tradeoffs = "word ".repeat(400);
        let harness = Harness::from_app(app_at((60, 12), asks(1, &long)));
        assert!(shows(&harness, "Recommended: SQLite"), "{}", harness.text());
        assert!(shows(&harness, "a answer"), "{}", harness.text());
    }

    #[test]
    fn inbox_draws_at_every_size_without_panicking_and_within_the_frame() {
        for (w, h) in [
            (0, 0),
            (1, 1),
            (3, 2),
            (10, 3),
            (20, 4),
            (40, 10),
            (80, 24),
            (200, 60),
        ] {
            for typing in [false, true] {
                let mut app = two_questions();
                app.size = (w, h);
                if typing {
                    app = typed(press(app, KeyCode::Char('a')), "an answer");
                    app.notice = Some("a notice".into());
                }
                let harness = Harness::from_app(app);
                assert_eq!(harness.text().lines().count().max(1), usize::from(h.max(1)));
                assert!(
                    rows(&harness)
                        .iter()
                        .all(|row| display_width(row) <= usize::from(w))
                );
            }
        }
    }

    #[test]
    fn inbox_the_header_still_names_the_screen() {
        let harness = Harness::from_app(waiting((80, 24)));
        assert!(
            harness.text().starts_with("6 Input inbox"),
            "{}",
            harness.text()
        );
    }

    // ---- carrying the answer out ----

    /// A project whose task 1 has asked `request` and is waiting.
    fn waiting_project(dir: &tempfile::TempDir) -> Project {
        let project = Project {
            root: dir.path().join("repo"),
            id: "inbox-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        std::fs::create_dir_all(&project.root).expect("create repo root");
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal
            .put_tasks(&[Task {
                id: TaskId::new(1),
                status: TaskStatus::Pending,
                body: "Task 1".to_string(),
                outcome: "outcome".to_string(),
                done_when: "done".to_string(),
                verify: "true".to_string(),
                refs: "none".to_string(),
                protocol: None,
            }])
            .expect("put tasks");
        for event in asks(1, &request()) {
            journal
                .append(Some(TaskId::new(1)), &event.kind)
                .expect("append");
        }
        project
    }

    /// The ADR `ktask-rs resolve --task 1 --note "Use SQLite."` writes for
    /// [`request`] on 2026-09-23, spelled out here independently of the code
    /// that writes it. `ktask-cli` pins the command's output to the same text.
    const CLI_ADR: &str = "# 0001. Postgres or SQLite for the journal\n\
\n\
- **Status:** accepted\n\
- **Date:** 2026-09-23\n\
\n\
## Context\n\
\n\
Task 1 paused for a decision (`waiting_input`):\n\
\n\
Postgres or SQLite for the journal?\n\
\n\
Trade-offs: Postgres scales; SQLite is one file.\n\
\n\
Recommended by the agent: SQLite\n\
\n\
## Decision\n\
\n\
Use SQLite.\n\
\n\
## Alternatives considered\n\
\n\
The options the agent put forward:\n\
\n\
- Postgres\n\
- SQLite\n\
\n\
## Consequences\n\
\n\
Journal durability and operational overhead.\n\
\n\
Recorded by `ktask-rs resolve`; task 1 runs again with this decision in its context.\n";

    #[test]
    fn inbox_an_answer_typed_in_the_interface_writes_the_adr_the_command_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        let app = typed(press(waiting((80, 24)), KeyCode::Char('a')), "Use SQLite.");
        let mut app = press(app, KeyCode::Enter);

        let sent = app.take_outbox();
        let [Action::Resolve { task, note }] = sent.as_slice() else {
            panic!("expected one resolve action, got {sent:?}");
        };
        let path = resolve(&project, *task, note, today()).expect("resolve");

        assert_eq!(
            path,
            PathBuf::from("docs/adr/0001-postgres-or-sqlite-for-the-journal.md")
        );
        let written = std::fs::read(project.root.join(&path)).expect("read the ADR");
        assert_eq!(written, CLI_ADR.as_bytes());
    }

    #[test]
    fn inbox_resolving_journals_the_answer_and_the_question_leaves_the_inbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);

        let path = resolve(&project, TaskId::new(1), "Use SQLite.", today()).expect("resolve");

        let journal = Journal::open_for(&project).expect("open journal");
        let events = journal.events_for(TaskId::new(1)).expect("events");
        let last = events.last().expect("an event");
        assert_eq!(
            last.kind,
            EventKind::DecisionResolved {
                adr_path: path,
                answer: "Use SQLite.".into(),
            }
        );
        let app = feed(waiting((80, 24)), vec![last.clone()]);
        assert!(app.inbox.is_empty());
    }

    #[test]
    fn inbox_resolve_trims_the_answer_like_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);

        let path =
            resolve(&project, TaskId::new(1), "\n  Use SQLite.  \n", today()).expect("resolve");

        let written = std::fs::read_to_string(project.root.join(path)).expect("read the ADR");
        assert_eq!(written, CLI_ADR);
    }

    #[test]
    fn inbox_resolve_refuses_an_empty_answer_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);

        let refused = resolve(&project, TaskId::new(1), "  \n", today());

        assert_eq!(refused, Err("resolve: the answer is empty".to_string()));
        assert!(!project.root.join("docs").exists());
    }

    #[test]
    fn inbox_resolve_refuses_a_task_that_asked_no_question() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);

        let refused = resolve(&project, TaskId::new(7), "x", today()).expect_err("no question");

        assert!(refused.starts_with("resolve: "), "{refused}");
        assert!(refused.contains("task 7"), "{refused}");
        assert!(refused.contains("no question"), "{refused}");
    }

    #[test]
    fn inbox_resolve_refuses_a_task_that_is_no_longer_waiting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        resolve(&project, TaskId::new(1), "Use SQLite.", today()).expect("first answer");

        let refused = resolve(&project, TaskId::new(1), "Again.", today()).expect_err("twice");

        assert!(
            refused.starts_with("resolve: task 1 cannot be resolved"),
            "{refused}"
        );
        let adrs = std::fs::read_dir(project.root.join("docs/adr"))
            .expect("adr dir")
            .count();
        assert_eq!(adrs, 1);
    }

    #[test]
    fn inbox_resolve_reports_an_unwritable_adr_with_the_answer_it_kept() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        let adrs = project.root.join("docs/adr");
        std::fs::create_dir_all(&adrs).expect("mkdir");
        std::fs::set_permissions(&adrs, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        let failed =
            resolve(&project, TaskId::new(1), "Use SQLite.", today()).expect_err("read-only");

        assert!(failed.starts_with("resolve: "), "{failed}");
        assert!(failed.contains("journaled"), "{failed}");
        assert!(failed.contains("Use SQLite."), "{failed}");
    }
}

//! The history screen: the event timeline across every attempt and remediation.
//!
//! The timeline is read from the journal, not remembered by the interface.
//! What [`History`] holds is a *page*: at most [`PAGE_ROWS`] rows of the
//! timeline, cut from the journal by [`backfill`], plus how many rows the whole
//! timeline has. Everything else stays in the journal, so a run with a million
//! events costs the interface the same as one with a hundred. [`fold`] does not
//! add events to anything; it only notes that the journal has moved on, so the
//! page is stale and [`wanted`] asks for a fresh one.
//!
//! [`update`](crate::update) does no I/O, so reading is the shell's part, as
//! for the live-run screen: after a turn it calls [`backfill`], which asks
//! [`wanted`] whether the page still covers what the view shows and, if not,
//! reads another. A read streams the queue's timeline with
//! [`Journal::for_each_event`] or reads the selected task's with
//! [`Journal::events_for`], keeping only the rows of the window it was asked
//! for. The queue-wide scan holds the window and one event; a task's own
//! events come back from the journal as one list, which is dropped as soon as
//! it has been walked. Scrolling therefore costs a journal pass, never a
//! growing page.
//!
//! Two timelines can be shown, switched with `t`: the whole queue, and the
//! task selected on the queue screen. Each event is a row with its timestamp
//! (UTC), the task (in the queue's timeline), the kind of event and the
//! logger's description of it ([`LogLine`]). Rows are grouped by attempt: a
//! heading opens each attempt of each task, and marks one begun by a retry as
//! a remediation. The events before a task's first attempt sit under a heading
//! of their own. The agent's output is left out; that is the logs screen.
//!
//! The view follows the newest row until it is scrolled up (`k`, `Up`, `g`,
//! `PageUp`), and follows again on reaching the last row (`j`, `Down`, `G`,
//! `PageDown`). While it is scrolled, its position is a row number in the
//! timeline, which stays where it is as the journal grows.
//!
//! Every description came from the journal and may carry text an agent
//! wrote, so it is sanitized and cut to one line when it is read.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::{LayoutPlan, layout_for};
use crate::sanitize::sanitize;
use crate::screen::queue::selected_row;
use crate::text::truncate_to_width;
use crate::types::Screen;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{AttemptId, Event, EventKind, EventSeq, Journal, Level, LogLine, Result, TaskId};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use time::{OffsetDateTime, UtcOffset};

/// The most rows a page holds, however long the timeline is.
#[cfg(not(test))]
pub const PAGE_ROWS: usize = 512;

/// Smaller under test, where every event of a long timeline is a journal
/// write that must reach the disk.
#[cfg(test)]
pub const PAGE_ROWS: usize = 64;

/// How many rows a page reaches either side of the row it is centred on.
const HALF_PAGE: usize = PAGE_ROWS / 2;

/// The longest description kept from an event.
const MAX_TEXT_CHARS: usize = 512;

/// What replaces the line breaks of a description.
const BREAK: &str = " ⏎ ";

/// The width of the column that names the kind of event: the longest kind.
const KIND_WIDTH: usize = 17;

/// The width of the column that names the task.
const TASK_WIDTH: usize = 6;

/// The marker column that points at the selected row.
const MARKER: &str = "> ";

/// The marker column of a row that is not selected.
const NO_MARKER: &str = "  ";

/// What separates the parts of the heading.
const SEPARATOR: &str = " · ";

/// What separates the entries of the key bar.
const BAR_GAP: &str = "  ";

/// What surrounds the text of an attempt heading.
const RULE: &str = "── ";

/// The fewest body rows that leave room for the heading and the key bar.
const CHROME_MIN_HEIGHT: u16 = 3;

/// What the body shows before the first page has been read.
const NOT_LOADED: &str = "Reading the journal…";

/// What the body shows when the timeline has no rows.
const EMPTY: &str = "No events recorded";

/// What the body shows on the task timeline when the queue has no task.
const NO_TASK: &str = "No task is selected";

/// Which timeline a page belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every event of the queue.
    Queue,
    /// The events of one task.
    Task(TaskId),
}

/// Which rows of the timeline a load keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// The last [`PAGE_ROWS`] rows.
    Tail,
    /// The [`PAGE_ROWS`] rows around this row number.
    Around(usize),
}

/// What [`wanted`] asks the shell to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The timeline to read.
    pub scope: Scope,
    /// The rows of it to keep.
    pub want: Want,
}

/// One event of the timeline, as it is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    ts: OffsetDateTime,
    task: Option<TaskId>,
    kind: &'static str,
    level: Level,
    text: String,
}

/// One row of the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// The heading that opens an attempt of a task, or the events before the
    /// task's first one.
    Group {
        task: TaskId,
        attempt: Option<AttemptId>,
        remediation: bool,
    },
    /// An event.
    Event(Entry),
}

/// A run of consecutive timeline rows read from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Page {
    scope: Scope,
    /// [`History::revision`] when the page was requested.
    revision: u64,
    /// The row number of the first row held.
    start: usize,
    /// How many rows the whole timeline has.
    total: usize,
    rows: Vec<Row>,
}

impl Page {
    /// Where the page ends: the row number after its last row.
    fn end(&self) -> usize {
        self.start + self.rows.len()
    }
}

/// What the history screen keeps: which timeline it shows, where the view is
/// in it, and the page read from the journal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct History {
    /// Whether the timeline is the selected task's, not the queue's.
    task_scope: bool,
    /// The row the view is on; `None` follows the newest row.
    cursor: Option<usize>,
    /// How many events have been folded that the timeline shows.
    revision: u64,
    page: Option<Page>,
}

impl History {
    /// Whether the view follows the newest row.
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.cursor.is_none()
    }

    /// Whether the timeline shown is the selected task's, not the queue's.
    #[must_use]
    pub fn is_task_scope(&self) -> bool {
        self.task_scope
    }

    /// How many rows the timeline has, as far as the page read says.
    #[must_use]
    pub fn len(&self) -> usize {
        self.page.as_ref().map_or(0, |page| page.total)
    }

    /// Whether the page read has no rows or none has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many rows the page holds in memory.
    #[must_use]
    pub fn held(&self) -> usize {
        self.page.as_ref().map_or(0, |page| page.rows.len())
    }
}

/// The context that decides which attempt an event is grouped under.
#[derive(Debug, Default)]
struct Grouper {
    /// The attempt each task is in, and whether a retry began it.
    attempts: BTreeMap<TaskId, Option<(AttemptId, bool)>>,
    /// The group of the last row, so a heading is written only on a change.
    last: Option<(TaskId, Option<AttemptId>)>,
}

impl Grouper {
    /// The rows `event` adds to the timeline: a heading if it opens a group,
    /// then the event. Agent output adds none.
    fn feed(&mut self, event: &Event) -> [Option<Row>; 2] {
        if matches!(event.kind, EventKind::AgentOutput { .. }) {
            return [None, None];
        }
        let line = LogLine::of(&event.kind);
        let entry = Row::Event(Entry {
            ts: event.ts,
            task: event.task_id,
            kind: event.kind.discriminant(),
            level: line.level,
            text: one_line(&line.message),
        });
        let Some(task) = event.task_id else {
            return [None, Some(entry)];
        };
        let slot = self.attempts.entry(task).or_insert(None);
        match &event.kind {
            EventKind::AttemptStarted { attempt, .. } => *slot = Some((*attempt, false)),
            EventKind::RetryStarted { attempt } => *slot = Some((*attempt, true)),
            EventKind::TaskQueued { .. } | EventKind::PreflightStarted => *slot = None,
            _ => {}
        }
        let key = (task, slot.map(|(attempt, _)| attempt));
        let heading = (self.last != Some(key)).then(|| Row::Group {
            task,
            attempt: key.1,
            remediation: slot.is_some_and(|(_, remediation)| remediation),
        });
        self.last = Some(key);
        [heading, Some(entry)]
    }
}

/// `message` sanitized, on one line and no longer than [`MAX_TEXT_CHARS`].
fn one_line(message: &str) -> String {
    let clean = sanitize(message);
    let joined = clean.trim_end_matches('\n').replace('\n', BREAK);
    match joined.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", joined.get(..end).unwrap_or_default()),
        None => joined,
    }
}

/// Collects the rows a [`Want`] keeps while a timeline is read through it.
#[derive(Debug)]
struct Window {
    grouper: Grouper,
    want: Want,
    /// The row number the next row will have.
    next: usize,
    /// The row number of the first row kept, for [`Want::Around`].
    from: usize,
    rows: VecDeque<Row>,
}

impl Window {
    fn new(want: Want) -> Self {
        Self {
            grouper: Grouper::default(),
            want,
            next: 0,
            from: match want {
                Want::Tail => 0,
                Want::Around(centre) => centre.saturating_sub(HALF_PAGE),
            },
            rows: VecDeque::new(),
        }
    }

    fn take(&mut self, event: &Event) {
        for row in self.grouper.feed(event).into_iter().flatten() {
            let at = self.next;
            self.next += 1;
            let keep = match self.want {
                Want::Tail => {
                    if self.rows.len() == PAGE_ROWS {
                        self.rows.pop_front();
                    }
                    true
                }
                Want::Around(_) => at >= self.from && at - self.from < PAGE_ROWS,
            };
            if keep {
                self.rows.push_back(row);
            }
        }
    }

    fn finish(self, scope: Scope, revision: u64) -> Page {
        let start = match self.want {
            Want::Tail => self.next - self.rows.len(),
            Want::Around(_) => self.from,
        };
        Page {
            scope,
            revision,
            start,
            total: self.next,
            rows: self.rows.into(),
        }
    }
}

/// Reads the timeline of `scope` from `journal`, keeping the rows `want` names.
///
/// The queue's timeline is streamed, so memory is the window plus one event.
/// A task's is read with [`Journal::events_for`], which returns the task's
/// events as one list; only the window is kept once it has been walked.
fn read(journal: &Journal, scope: Scope, want: Want, revision: u64) -> Result<Page> {
    let mut window = Window::new(want);
    match scope {
        Scope::Queue => journal.for_each_event(EventSeq::new(0), &mut |event| {
            window.take(&event);
            Ok(())
        })?,
        Scope::Task(task) => {
            for event in journal.events_for(task)? {
                window.take(&event);
            }
        }
    }
    Ok(window.finish(scope, revision))
}

/// The timeline `app` should show: the queue's, or the task selected on the
/// queue screen. `None` on the task timeline when the queue has no task.
fn scope_of(app: &App) -> Option<Scope> {
    if !app.history.task_scope {
        return Some(Scope::Queue);
    }
    let task = app.tasks.get(selected_row(app)?)?;
    Some(Scope::Task(task.id))
}

/// How many timeline rows the body of `app` has room for.
fn rows_height(app: &App) -> usize {
    let (columns, rows) = app.size;
    let body = layout_for(Rect::new(0, 0, columns, rows)).body;
    let chrome = if body.height >= CHROME_MIN_HEIGHT {
        2
    } else {
        0
    };
    usize::from(body.height - chrome).min(HALF_PAGE)
}

/// The row numbers the view shows: the newest `height` rows when following,
/// otherwise the ones around `cursor`, never past the ends.
pub(crate) fn viewport(cursor: Option<usize>, total: usize, height: usize) -> Range<usize> {
    let last_top = total.saturating_sub(height);
    let top = match cursor {
        None => last_top,
        Some(row) => row
            .min(total.saturating_sub(1))
            .saturating_sub(height / 2)
            .min(last_top),
    };
    top..(top + height).min(total)
}

/// What the shell should read to make the page cover the view, or `None` when
/// it already does, or when this screen is not showing.
///
/// A page is wanted when none has been read, when it is of another timeline,
/// when the journal has moved on since it was read, and when the view has
/// scrolled out of it.
#[must_use]
pub fn wanted(app: &App) -> Option<Request> {
    if app.screen != Screen::History {
        return None;
    }
    let history = &app.history;
    let scope = scope_of(app)?;
    let request = Request {
        scope,
        want: history.cursor.map_or(Want::Tail, Want::Around),
    };
    let Some(page) = &history.page else {
        return Some(request);
    };
    if page.scope != scope || page.revision != history.revision {
        return Some(request);
    }
    let view = viewport(history.cursor, page.total, rows_height(app));
    let covered = page.start <= view.start && view.end <= page.end();
    (!covered).then_some(request)
}

/// Reads from `journal` the page [`wanted`] asks for and stores it, replacing
/// the previous one. Returns whether it read.
///
/// The shell calls this after a turn; it is the one place this screen does
/// I/O.
///
/// # Errors
///
/// See [`Journal::for_each_event`] and [`Journal::events_for`]. The page is
/// unchanged on error.
pub fn backfill(app: &mut App, journal: &Journal) -> Result<bool> {
    let Some(request) = wanted(app) else {
        return Ok(false);
    };
    let revision = app.history.revision;
    app.history.page = Some(read(journal, request.scope, request.want, revision)?);
    Ok(true)
}

/// Notes that the journal has an event the timeline may show. The event itself
/// is not kept: the next page read finds it in the journal.
pub fn fold(app: &mut App, event: &Event) {
    if !matches!(event.kind, EventKind::AgentOutput { .. }) {
        app.history.revision = app.history.revision.wrapping_add(1);
    }
}

/// Whether the history has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::History && app.overlay.is_none()
}

/// Handles the history's keys: `j`, `k`, the arrows, `g` and `G` move the
/// view a row or to either end, `PageUp` and `PageDown` a screenful, and `t`
/// switches between the queue's timeline and the selected task's. Does nothing
/// on other screens or under an overlay.
///
/// Moving stops at the ends rather than wrapping, and reaching the last row
/// follows the newest again.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Char('t') {
        app.history.task_scope = !app.history.task_scope;
        app.history.cursor = None;
        return;
    }
    let Some(last) = app.history.len().checked_sub(1) else {
        return;
    };
    let current = app.history.cursor.map_or(last, |row| row.min(last));
    let screenful = rows_height(app).max(1);
    let target = match (key.code, lookup(app.screen, key).map(|b| b.action)) {
        (KeyCode::PageUp, _) => current.saturating_sub(screenful),
        (KeyCode::PageDown, _) => current.saturating_add(screenful),
        (_, Some(KeyAction::MoveUp)) => current.saturating_sub(1),
        (_, Some(KeyAction::MoveDown)) => current.saturating_add(1),
        (_, Some(KeyAction::First)) => 0,
        (_, Some(KeyAction::Last)) => last,
        _ => return,
    };
    app.history.cursor = (target < last).then_some(target);
}

/// `ts` as `YYYY-MM-DD HH:MM:SS`, in UTC.
fn stamp(ts: OffsetDateTime) -> String {
    let ts = ts.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        ts.year(),
        u8::from(ts.month()),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second()
    )
}

/// The style an event of `level` is drawn in.
fn level_style(level: Level) -> Style {
    match level {
        Level::Debug => Style::new().add_modifier(Modifier::DIM),
        Level::Info => Style::new(),
        Level::Warn => Style::new().fg(Color::Yellow),
        Level::Error => Style::new().fg(Color::Red),
    }
}

/// The text of a group heading.
fn group_text(
    task: TaskId,
    attempt: Option<AttemptId>,
    remediation: bool,
    in_queue: bool,
) -> String {
    let what = match attempt {
        Some(attempt) if remediation => format!("attempt {attempt} (remediation)"),
        Some(attempt) => format!("attempt {attempt}"),
        None => "before any attempt".to_owned(),
    };
    if in_queue {
        format!("{RULE}task {task}{SEPARATOR}{what}")
    } else {
        format!("{RULE}{what}")
    }
}

/// The text of an event row after its marker.
fn entry_text(entry: &Entry, in_queue: bool) -> String {
    let task = match (in_queue, entry.task) {
        (true, Some(task)) => format!("T{task}"),
        _ => String::new(),
    };
    let task_column = if in_queue {
        format!("{task:<TASK_WIDTH$}")
    } else {
        String::new()
    };
    format!(
        "{} {task_column}{:<KIND_WIDTH$} {}",
        stamp(entry.ts),
        entry.kind,
        entry.text
    )
}

/// The row as a line: marker, then the heading or event, cut at `width`.
fn row_line(row: &Row, selected: bool, in_queue: bool, width: usize) -> Line<'static> {
    let marker = if selected { MARKER } else { NO_MARKER };
    let (text, style) = match row {
        Row::Group {
            task,
            attempt,
            remediation,
        } => (
            group_text(*task, *attempt, *remediation, in_queue),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Row::Event(entry) => (entry_text(entry, in_queue), level_style(entry.level)),
    };
    let style = if selected {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    Line::styled(truncate_to_width(&format!("{marker}{text}"), width), style)
}

/// The heading: which timeline, where the view is in it and whether it follows.
fn heading(app: &App, scope: Option<Scope>) -> String {
    let history = &app.history;
    let mut parts = vec!["History".to_owned()];
    parts.push(match scope {
        Some(Scope::Task(task)) => format!("task {task}"),
        Some(Scope::Queue) | None if !history.task_scope => "whole queue".to_owned(),
        Some(Scope::Queue) | None => "no task".to_owned(),
    });
    let total = history.len();
    if total > 0 {
        let row = history.cursor.map_or(total, |row| row.min(total - 1) + 1);
        parts.push(format!("line {row} of {total}"));
        parts.push(if history.is_following() {
            "following".to_owned()
        } else {
            "scrolled (G to follow)".to_owned()
        });
    }
    parts.join(SEPARATOR)
}

/// The line of keys shown under the timeline.
fn key_bar(task_scope: bool) -> String {
    let other = if task_scope { "queue" } else { "task" };
    [
        "j/k scroll".to_owned(),
        "PgUp/PgDn page".to_owned(),
        "g/G first/last".to_owned(),
        format!("t {other} timeline"),
    ]
    .join(BAR_GAP)
}

/// Draws the history into the body of `plan`: a heading, the rows of the page
/// the view shows, and the keys. The heading and the keys appear where the body
/// is tall enough for them.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let chrome = u16::from(body.height >= CHROME_MIN_HEIGHT);
    let [heading_area, rows_area, bar_area] = Layout::vertical([
        Constraint::Length(chrome),
        Constraint::Fill(1),
        Constraint::Length(chrome),
    ])
    .areas(body);
    let history = &app.history;
    let scope = scope_of(app);
    frame.render_widget(
        Paragraph::new(truncate_to_width(&heading(app, scope), width))
            .style(Style::new().add_modifier(Modifier::BOLD)),
        heading_area,
    );
    frame.render_widget(
        Paragraph::new(truncate_to_width(&key_bar(history.task_scope), width)),
        bar_area,
    );
    let dim = Style::new().add_modifier(Modifier::DIM);
    let page = history
        .page
        .as_ref()
        .filter(|page| Some(page.scope) == scope);
    let message = match (scope, page) {
        (None, _) => Some(NO_TASK),
        (Some(_), None) => Some(NOT_LOADED),
        (Some(_), Some(page)) if page.total == 0 => Some(EMPTY),
        (Some(_), Some(_)) => None,
    };
    if let Some(message) = message {
        frame.render_widget(
            Paragraph::new(truncate_to_width(message, width)).style(dim),
            rows_area,
        );
        return;
    }
    let Some(page) = page else { return };
    let height = usize::from(rows_area.height).min(HALF_PAGE);
    let view = viewport(history.cursor, page.total, height);
    let selected = history
        .cursor
        .map_or(page.total - 1, |row| row.min(page.total - 1));
    let in_queue = matches!(page.scope, Scope::Queue);
    let lines: Vec<Line<'static>> = view
        .filter_map(|at| {
            let row = page.rows.get(at.checked_sub(page.start)?)?;
            Some(row_line(row, at == selected, in_queue, width))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::update;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::TaskView;
    use ktask_core::{FailureClass, GateKind, GateResult, PauseReason, Phase, Stream};
    use time::macros::datetime;

    fn event(task: Option<u32>, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(1),
            ts: datetime!(2026-09-23 10:30:05 UTC),
            task_id: task.map(TaskId::new),
            kind,
        }
    }

    fn queued(task: u32) -> Event {
        event(
            Some(task),
            EventKind::TaskQueued {
                title: format!("Task number {task}"),
            },
        )
    }

    fn attempt(task: u32, attempt: u32) -> Event {
        event(
            Some(task),
            EventKind::AttemptStarted {
                attempt: AttemptId::new(attempt),
                protocol: "tdd".into(),
                pid: 7,
                base_sha: "abc".into(),
            },
        )
    }

    fn retry(task: u32, attempt: u32) -> Event {
        event(
            Some(task),
            EventKind::RetryStarted {
                attempt: AttemptId::new(attempt),
            },
        )
    }

    fn phase(task: u32, phase: Phase) -> Event {
        event(
            Some(task),
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase,
            },
        )
    }

    fn output(task: u32, text: &str) -> Event {
        event(
            Some(task),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: text.into(),
            },
        )
    }

    fn gate(task: u32, passed: bool) -> Event {
        event(
            Some(task),
            EventKind::GateFinished {
                result: GateResult {
                    kind: GateKind::Verify,
                    passed,
                    exit_code: Some(i32::from(!passed)),
                    signal: None,
                    duration_ms: 5,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                },
            },
        )
    }

    fn failed(task: u32) -> Event {
        event(
            Some(task),
            EventKind::TaskFailed {
                class: FailureClass::VerificationFailure,
                detail: "clippy failed".into(),
            },
        )
    }

    fn rows_of(events: &[Event]) -> Vec<Row> {
        let mut grouper = Grouper::default();
        events
            .iter()
            .flat_map(|event| grouper.feed(event))
            .flatten()
            .collect()
    }

    fn group(task: u32, attempt: Option<u32>, remediation: bool) -> Row {
        Row::Group {
            task: TaskId::new(task),
            attempt: attempt.map(AttemptId::new),
            remediation,
        }
    }

    fn kinds(rows: &[Row]) -> Vec<&'static str> {
        rows.iter()
            .map(|row| match row {
                Row::Group { .. } => "--",
                Row::Event(entry) => entry.kind,
            })
            .collect()
    }

    /// A journal in a directory of its own, removed on drop.
    struct Scratch {
        _dir: tempfile::TempDir,
        journal: Journal,
    }

    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&dir.path().join("journal.db")).expect("open");
        Scratch { _dir: dir, journal }
    }

    impl Scratch {
        fn add(&mut self, event: &Event) {
            self.journal
                .append(event.task_id, &event.kind)
                .expect("append");
        }
    }

    fn app_on_history(size: (u16, u16)) -> App {
        App {
            screen: Screen::History,
            ..App::new(size)
        }
    }

    fn with_tasks(mut app: App, count: u32) -> App {
        for id in 1..=count {
            app.tasks.push(TaskView {
                id: TaskId::new(id),
                title: format!("Task number {id}"),
                state: "Queued".into(),
                protocol: String::new(),
                phase: None,
                attempts: 0,
                elapsed: None,
            });
        }
        app
    }

    fn feed(app: App, events: &[Event]) -> App {
        events
            .iter()
            .cloned()
            .fold(app, |app, event| update(app, AppEvent::Core(event)))
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn text_of(app: &App) -> String {
        Harness::from_app(app.clone()).text()
    }

    fn lines_of(app: &App) -> Vec<String> {
        text_of(app).lines().map(str::to_owned).collect()
    }

    /// An app on the history screen whose page was read from `journal`.
    fn loaded(size: (u16, u16), journal: &Journal, tasks: u32) -> App {
        let mut app = with_tasks(app_on_history(size), tasks);
        backfill(&mut app, journal).expect("backfill");
        app
    }

    // ---- what is a row ----

    #[test]
    fn history_a_task_queued_opens_a_group_before_any_attempt_then_shows_the_event() {
        let rows = rows_of(&[queued(1)]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.first(), Some(&group(1, None, false)));
        let Some(Row::Event(entry)) = rows.get(1) else {
            panic!("second row is an event: {rows:?}");
        };
        assert_eq!(entry.kind, "TaskQueued");
        assert_eq!(entry.task, Some(TaskId::new(1)));
        assert_eq!(entry.ts, datetime!(2026-09-23 10:30:05 UTC));
        assert_eq!(entry.level, Level::Info);
        assert_eq!(entry.text, "task queued: Task number 1");
    }

    #[test]
    fn history_events_are_grouped_by_attempt_and_a_retry_opens_a_remediation() {
        let rows = rows_of(&[
            queued(1),
            attempt(1, 1),
            phase(1, Phase::Red),
            failed(1),
            retry(1, 2),
            phase(1, Phase::Green),
        ]);
        assert_eq!(
            kinds(&rows),
            [
                "--",
                "TaskQueued",
                "--",
                "AttemptStarted",
                "PhaseEntered",
                "TaskFailed",
                "--",
                "RetryStarted",
                "PhaseEntered",
            ]
        );
        assert_eq!(rows.first(), Some(&group(1, None, false)));
        assert_eq!(rows.get(2), Some(&group(1, Some(1), false)));
        assert_eq!(rows.get(6), Some(&group(1, Some(2), true)));
    }

    #[test]
    fn history_a_new_preflight_leaves_the_previous_attempt_group() {
        let rows = rows_of(&[
            queued(1),
            attempt(1, 1),
            event(Some(1), EventKind::PreflightStarted),
        ]);
        assert_eq!(
            kinds(&rows),
            [
                "--",
                "TaskQueued",
                "--",
                "AttemptStarted",
                "--",
                "PreflightStarted"
            ]
        );
        assert_eq!(rows.get(4), Some(&group(1, None, false)));
    }

    #[test]
    fn history_each_task_keeps_its_own_attempt_when_events_interleave() {
        let rows = rows_of(&[attempt(1, 1), attempt(2, 1), gate(1, true)]);
        assert_eq!(
            rows.iter()
                .filter_map(|row| match row {
                    Row::Group { task, attempt, .. } =>
                        Some((task.get(), attempt.map(AttemptId::get))),
                    Row::Event(_) => None,
                })
                .collect::<Vec<_>>(),
            [(1, Some(1)), (2, Some(1)), (1, Some(1))]
        );
    }

    #[test]
    fn history_no_new_group_while_the_attempt_does_not_change() {
        let rows = rows_of(&[attempt(1, 1), gate(1, true), gate(1, false)]);
        assert_eq!(
            kinds(&rows),
            ["--", "AttemptStarted", "GateFinished", "GateFinished"]
        );
    }

    #[test]
    fn history_an_event_without_a_task_has_no_group_and_keeps_the_last_one() {
        let rows = rows_of(&[
            attempt(1, 1),
            event(None, EventKind::Resumed),
            gate(1, true),
        ]);
        assert_eq!(
            kinds(&rows),
            ["--", "AttemptStarted", "Resumed", "GateFinished"]
        );
    }

    #[test]
    fn history_agent_output_is_not_part_of_the_timeline() {
        let rows = rows_of(&[attempt(1, 1), output(1, "line"), output(1, "more")]);
        assert_eq!(kinds(&rows), ["--", "AttemptStarted"]);
    }

    #[test]
    fn history_levels_follow_the_logger() {
        let rows = rows_of(&[
            gate(1, true),
            failed(1),
            event(
                Some(1),
                EventKind::Paused {
                    reason: PauseReason::Input,
                },
            ),
        ]);
        let levels: Vec<Level> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Event(entry) => Some(entry.level),
                Row::Group { .. } => None,
            })
            .collect();
        assert_eq!(levels, [Level::Info, Level::Error, Level::Warn]);
    }

    #[test]
    fn history_descriptions_are_sanitized_on_one_line_and_bounded() {
        let evil = event(
            Some(1),
            EventKind::TaskQueued {
                title: format!(
                    "a\x1b[31mred\x1b[0m\nsecond {}",
                    "x".repeat(2 * MAX_TEXT_CHARS)
                ),
            },
        );
        let rows = rows_of(&[evil]);
        let Some(Row::Event(entry)) = rows.get(1) else {
            panic!("an event row");
        };
        assert!(!entry.text.contains('\x1b'), "{:?}", entry.text);
        assert!(!entry.text.contains('\n'));
        assert!(entry.text.contains("ared ⏎ second"), "{:?}", entry.text);
        assert_eq!(entry.text.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(entry.text.ends_with('…'));
    }

    // ---- the page is read from the journal ----

    #[test]
    fn history_the_queue_timeline_is_read_from_the_journal_across_tasks() {
        let mut journal = scratch();
        for event in [queued(1), attempt(1, 1), queued(2), attempt(2, 1)] {
            journal.add(&event);
        }
        let page = read(&journal.journal, Scope::Queue, Want::Tail, 0).expect("read");
        assert_eq!(page.scope, Scope::Queue);
        assert_eq!(page.start, 0);
        assert_eq!(page.total, 8);
        assert_eq!(
            kinds(&page.rows),
            [
                "--",
                "TaskQueued",
                "--",
                "AttemptStarted",
                "--",
                "TaskQueued",
                "--",
                "AttemptStarted"
            ]
        );
    }

    #[test]
    fn history_a_task_timeline_holds_only_that_tasks_events() {
        let mut journal = scratch();
        for event in [
            queued(1),
            queued(2),
            attempt(2, 1),
            gate(2, true),
            attempt(1, 1),
        ] {
            journal.add(&event);
        }
        let page =
            read(&journal.journal, Scope::Task(TaskId::new(2)), Want::Tail, 0).expect("read");
        assert_eq!(page.scope, Scope::Task(TaskId::new(2)));
        assert_eq!(page.total, 5);
        assert_eq!(
            kinds(&page.rows),
            ["--", "TaskQueued", "--", "AttemptStarted", "GateFinished"]
        );
    }

    /// `count` events of no task, each titled with its number, so a row says
    /// which event it is.
    fn numbered(journal: &mut Scratch, count: usize) {
        for n in 0..count {
            journal.add(&event(
                None,
                EventKind::TaskQueued {
                    title: n.to_string(),
                },
            ));
        }
    }

    fn number_of(row: &Row) -> usize {
        let Row::Event(entry) = row else {
            panic!("an event row: {row:?}");
        };
        entry
            .text
            .strip_prefix("task queued: ")
            .and_then(|n| n.parse().ok())
            .expect("a numbered event")
    }

    const LONG: usize = PAGE_ROWS + 88;

    #[test]
    fn history_the_tail_of_a_long_timeline_is_a_bounded_page_of_the_newest_rows() {
        let mut journal = scratch();
        numbered(&mut journal, LONG);
        let page = read(&journal.journal, Scope::Queue, Want::Tail, 0).expect("read");
        assert_eq!(page.total, LONG);
        assert_eq!(page.rows.len(), PAGE_ROWS);
        assert_eq!(page.start, LONG - PAGE_ROWS);
        assert_eq!(page.end(), LONG);
        assert_eq!(page.rows.first().map(number_of), Some(LONG - PAGE_ROWS));
        assert_eq!(page.rows.last().map(number_of), Some(LONG - 1));
    }

    #[test]
    fn history_a_page_around_a_row_is_bounded_and_starts_half_a_page_before_it() {
        let mut journal = scratch();
        numbered(&mut journal, LONG);
        let centre = LONG / 2;
        let page = read(&journal.journal, Scope::Queue, Want::Around(centre), 0).expect("read");
        assert_eq!(page.total, LONG);
        assert_eq!(page.start, centre - HALF_PAGE);
        assert_eq!(page.rows.len(), PAGE_ROWS);
        assert_eq!(page.rows.first().map(number_of), Some(centre - HALF_PAGE));
        assert_eq!(
            page.rows.last().map(number_of),
            Some(centre - HALF_PAGE + PAGE_ROWS - 1)
        );
    }

    #[test]
    fn history_a_page_around_a_row_near_the_start_begins_at_the_first_row() {
        let mut journal = scratch();
        numbered(&mut journal, LONG);
        let page = read(&journal.journal, Scope::Queue, Want::Around(3), 0).expect("read");
        assert_eq!(page.start, 0);
        assert_eq!(page.rows.first().map(number_of), Some(0));
        assert_eq!(page.rows.len(), PAGE_ROWS);
    }

    #[test]
    fn history_a_page_around_a_row_near_the_end_holds_what_is_left() {
        let mut journal = scratch();
        numbered(&mut journal, LONG);
        let page = read(&journal.journal, Scope::Queue, Want::Around(LONG - 1), 0).expect("read");
        assert_eq!(page.start, LONG - 1 - HALF_PAGE);
        assert_eq!(page.rows.len(), HALF_PAGE + 1);
        assert_eq!(page.end(), LONG);
    }

    #[test]
    fn history_a_short_timeline_is_held_whole() {
        let mut journal = scratch();
        numbered(&mut journal, 5);
        let page = read(&journal.journal, Scope::Queue, Want::Tail, 0).expect("read");
        assert_eq!((page.start, page.total, page.rows.len()), (0, 5, 5));
    }

    #[test]
    fn history_an_empty_journal_reads_an_empty_page() {
        let journal = scratch();
        let page = read(&journal.journal, Scope::Queue, Want::Tail, 0).expect("read");
        assert_eq!((page.start, page.total, page.rows.len()), (0, 0, 0));
    }

    #[test]
    fn history_a_page_remembers_the_revision_it_was_read_at() {
        let journal = scratch();
        let page = read(&journal.journal, Scope::Queue, Want::Tail, 41).expect("read");
        assert_eq!(page.revision, 41);
    }

    // ---- what the interface keeps ----

    #[test]
    fn history_folding_events_keeps_none_of_them() {
        let mut app = app_on_history((80, 24));
        for n in 0..10_000 {
            app = update(
                app,
                AppEvent::Core(event(
                    None,
                    EventKind::TaskQueued {
                        title: n.to_string(),
                    },
                )),
            );
        }
        assert_eq!(app.history.held(), 0);
        assert_eq!(app.history.len(), 0);
        assert_eq!(app.history.revision, 10_000);
    }

    #[test]
    fn history_agent_output_does_not_make_the_page_stale() {
        let app = feed(app_on_history((80, 24)), &[output(1, "a"), output(1, "b")]);
        assert_eq!(app.history.revision, 0);
    }

    #[test]
    fn history_backfilling_a_long_journal_holds_a_bounded_page() {
        let mut journal = scratch();
        numbered(&mut journal, LONG);
        let app = loaded((80, 24), &journal.journal, 0);
        assert_eq!(app.history.len(), LONG);
        assert_eq!(app.history.held(), PAGE_ROWS);
        assert!(app.history.held() < app.history.len());
    }

    // ---- when the shell must read ----

    #[test]
    fn history_wants_the_tail_before_anything_has_been_read() {
        let app = app_on_history((80, 24));
        assert_eq!(
            wanted(&app),
            Some(Request {
                scope: Scope::Queue,
                want: Want::Tail
            })
        );
    }

    #[test]
    fn history_wants_nothing_on_another_screen() {
        let app = App::new((80, 24));
        assert_eq!(app.screen, Screen::Queue);
        assert_eq!(wanted(&app), None);
        let mut journal = scratch();
        numbered(&mut journal, 3);
        let mut app = app;
        assert!(!backfill(&mut app, &journal.journal).expect("backfill"));
        assert_eq!(app.history.held(), 0);
    }

    #[test]
    fn history_wants_nothing_once_the_page_covers_the_view() {
        let mut journal = scratch();
        numbered(&mut journal, 40);
        let mut app = loaded((80, 24), &journal.journal, 0);
        assert_eq!(wanted(&app), None);
        assert!(!backfill(&mut app, &journal.journal).expect("backfill"));
    }

    #[test]
    fn history_wants_a_new_page_when_the_journal_has_moved_on() {
        let mut journal = scratch();
        numbered(&mut journal, 40);
        let mut app = loaded((80, 24), &journal.journal, 0);
        assert_eq!(app.history.len(), 40);
        journal.add(&event(None, EventKind::Resumed));
        app = feed(app, &[event(None, EventKind::Resumed)]);
        assert!(wanted(&app).is_some());
        assert!(backfill(&mut app, &journal.journal).expect("backfill"));
        assert_eq!(app.history.len(), 41);
        assert_eq!(wanted(&app), None);
    }

    #[test]
    fn history_the_timeline_comes_from_the_journal_not_from_the_events_folded() {
        let mut journal = scratch();
        numbered(&mut journal, 4);
        // The interface is told of one event that the journal does not have.
        let mut app = feed(
            app_on_history((80, 24)),
            &[event(
                None,
                EventKind::TaskQueued {
                    title: "phantom".into(),
                },
            )],
        );
        assert!(backfill(&mut app, &journal.journal).expect("backfill"));
        assert_eq!(app.history.len(), 4);
        assert!(!text_of(&app).contains("phantom"));
    }

    #[test]
    fn history_wants_another_timeline_after_the_scope_switches() {
        let mut journal = scratch();
        numbered(&mut journal, 3);
        let mut app = loaded((80, 24), &journal.journal, 2);
        app = press(app, KeyCode::Char('t'));
        assert_eq!(
            wanted(&app),
            Some(Request {
                scope: Scope::Task(TaskId::new(1)),
                want: Want::Tail
            })
        );
    }

    #[test]
    fn history_wants_another_task_when_the_queue_selects_another() {
        let journal = scratch();
        let mut app = with_tasks(app_on_history((80, 24)), 3);
        app.history.task_scope = true;
        backfill(&mut app, &journal.journal).expect("backfill");
        assert_eq!(wanted(&app), None);
        app.selected.insert(Screen::Queue, 2);
        assert_eq!(
            wanted(&app),
            Some(Request {
                scope: Scope::Task(TaskId::new(3)),
                want: Want::Tail
            })
        );
    }

    #[test]
    fn history_the_task_timeline_wants_nothing_when_the_queue_has_no_task() {
        let mut app = app_on_history((80, 24));
        app.history.task_scope = true;
        assert_eq!(wanted(&app), None);
    }

    #[test]
    fn history_scrolling_out_of_the_page_wants_a_page_around_the_view() {
        let mut journal = scratch();
        numbered(&mut journal, 3 * PAGE_ROWS);
        let mut app = loaded((80, 24), &journal.journal, 0);
        assert_eq!(
            app.history.page.as_ref().map(|p| p.start),
            Some(2 * PAGE_ROWS)
        );
        app = press(app, KeyCode::Char('g'));
        assert_eq!(
            wanted(&app),
            Some(Request {
                scope: Scope::Queue,
                want: Want::Around(0)
            })
        );
        assert!(backfill(&mut app, &journal.journal).expect("backfill"));
        assert_eq!(app.history.page.as_ref().map(|p| p.start), Some(0));
        assert_eq!(wanted(&app), None);
    }

    #[test]
    fn history_scrolling_inside_the_page_reads_nothing() {
        let mut journal = scratch();
        numbered(&mut journal, 3 * PAGE_ROWS);
        let mut app = loaded((80, 24), &journal.journal, 0);
        for _ in 0..40 {
            app = press(app, KeyCode::Char('k'));
        }
        assert_eq!(wanted(&app), None);
    }

    #[test]
    fn history_a_cursor_past_the_end_shows_the_newest_rows_and_reads_nothing() {
        let mut journal = scratch();
        numbered(&mut journal, 3 * PAGE_ROWS);
        let mut app = loaded((80, 24), &journal.journal, 0);
        app.history.cursor = Some(10 * PAGE_ROWS);
        assert_eq!(wanted(&app), None);
        let total = 3 * PAGE_ROWS;
        assert!(
            lines_of(&app)
                .iter()
                .any(|l| l.contains(&format!("line {total} of {total}"))),
            "{:#?}",
            lines_of(&app)
        );
        let last = format!("task queued: {}", total - 1);
        assert!(
            lines_of(&app)
                .iter()
                .any(|l| l.starts_with(MARKER) && l.trim_end().ends_with(&last)),
            "{:#?}",
            lines_of(&app)
        );
    }

    // ---- moving ----

    fn timeline_app(rows: usize, size: (u16, u16)) -> (Scratch, App) {
        let mut journal = scratch();
        numbered(&mut journal, rows);
        let app = loaded(size, &journal.journal, 0);
        (journal, app)
    }

    #[test]
    fn history_len_and_is_empty_say_how_many_rows_the_timeline_has() {
        let fresh = app_on_history((80, 24));
        assert_eq!(fresh.history.len(), 0);
        assert!(fresh.history.is_empty());
        let (_journal, app) = timeline_app(5, (80, 24));
        assert_eq!(app.history.len(), 5);
        assert!(!app.history.is_empty());
    }

    #[test]
    fn history_starts_following_the_newest_row() {
        let (_journal, app) = timeline_app(50, (80, 24));
        assert!(app.history.is_following());
        assert_eq!(app.history.cursor, None);
    }

    #[test]
    fn history_k_scrolls_up_a_row_and_stops_following() {
        let (_journal, app) = timeline_app(50, (80, 24));
        let app = press(app, KeyCode::Char('k'));
        assert_eq!(app.history.cursor, Some(48));
        assert!(!app.history.is_following());
        let app = press(app, KeyCode::Up);
        assert_eq!(app.history.cursor, Some(47));
    }

    #[test]
    fn history_j_scrolls_down_and_follows_again_at_the_last_row() {
        let (_journal, app) = timeline_app(50, (80, 24));
        let app = press(
            press(press(app, KeyCode::Char('k')), KeyCode::Char('k')),
            KeyCode::Char('k'),
        );
        assert_eq!(app.history.cursor, Some(46));
        let app = press(app, KeyCode::Char('j'));
        assert_eq!(app.history.cursor, Some(47));
        let app = press(app, KeyCode::Down);
        assert_eq!(app.history.cursor, Some(48));
        let app = press(app, KeyCode::Char('j'));
        assert_eq!(app.history.cursor, None);
        assert!(app.history.is_following());
    }

    #[test]
    fn history_j_on_the_newest_row_stays_following() {
        let (_journal, app) = timeline_app(50, (80, 24));
        let app = press(app, KeyCode::Char('j'));
        assert_eq!(app.history.cursor, None);
    }

    #[test]
    fn history_k_stops_at_the_first_row() {
        let (_journal, app) = timeline_app(3, (80, 24));
        let app = (0..10).fold(app, |app, _| press(app, KeyCode::Char('k')));
        assert_eq!(app.history.cursor, Some(0));
    }

    #[test]
    fn history_g_goes_to_the_first_row_and_capital_g_follows_the_newest() {
        let (_journal, app) = timeline_app(50, (80, 24));
        let app = press(app, KeyCode::Char('g'));
        assert_eq!(app.history.cursor, Some(0));
        let app = press(app, KeyCode::Char('G'));
        assert_eq!(app.history.cursor, None);
    }

    #[test]
    fn history_page_up_and_page_down_move_a_screenful() {
        let (_journal, app) = timeline_app(100, (80, 24));
        // 24 rows less the header, footer, heading and key bar leave 20.
        assert_eq!(rows_height(&app), 20);
        let app = press(app, KeyCode::PageUp);
        assert_eq!(app.history.cursor, Some(99 - 20));
        let app = press(app, KeyCode::PageUp);
        assert_eq!(app.history.cursor, Some(99 - 40));
        let app = press(app, KeyCode::PageDown);
        assert_eq!(app.history.cursor, Some(99 - 20));
        let app = press(press(app, KeyCode::PageDown), KeyCode::PageDown);
        assert_eq!(app.history.cursor, None);
    }

    #[test]
    fn history_page_up_stops_at_the_first_row() {
        let (_journal, app) = timeline_app(30, (80, 24));
        let app = press(press(app, KeyCode::PageUp), KeyCode::PageUp);
        assert_eq!(app.history.cursor, Some(0));
    }

    #[test]
    fn history_keys_do_nothing_when_there_are_no_rows() {
        let app = app_on_history((80, 24));
        let before = app.clone();
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            assert_eq!(press(app.clone(), code), before, "{code:?}");
        }
    }

    #[test]
    fn history_t_switches_between_the_queue_and_the_task_and_follows_again() {
        let (_journal, app) = timeline_app(50, (80, 24));
        let app = press(app, KeyCode::Char('k'));
        assert!(app.history.cursor.is_some());
        let app = press(app, KeyCode::Char('t'));
        assert!(app.history.is_task_scope());
        assert!(app.history.is_following());
        let app = press(app, KeyCode::Char('t'));
        assert!(!app.history.is_task_scope());
    }

    #[test]
    fn history_keys_are_ignored_on_other_screens_and_under_an_overlay() {
        let (_journal, app) = timeline_app(50, (80, 24));
        let elsewhere = App {
            screen: Screen::Git,
            ..app.clone()
        };
        assert_eq!(
            press(elsewhere.clone(), KeyCode::Char('k')).history,
            elsewhere.history
        );
        assert_eq!(
            press(elsewhere.clone(), KeyCode::Char('t')).history,
            elsewhere.history
        );
        let covered = App {
            overlay: Some(crate::types::Overlay::KeyMap),
            ..app
        };
        assert_eq!(
            press(covered.clone(), KeyCode::Char('k')).history,
            covered.history
        );
        assert_eq!(
            press(covered.clone(), KeyCode::Char('t')).history,
            covered.history
        );
    }

    #[test]
    fn history_t_with_a_modifier_is_not_the_scope_key() {
        let (_journal, app) = timeline_app(5, (80, 24));
        let app = update(
            app,
            AppEvent::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
        );
        assert!(!app.history.is_task_scope());
    }

    #[test]
    fn history_number_keys_still_change_screens() {
        let (_journal, app) = timeline_app(5, (80, 24));
        let app = press(app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Queue);
    }

    // ---- the view ----

    #[test]
    fn history_viewport_follows_the_end_or_centres_the_cursor_within_the_ends() {
        assert_eq!(viewport(None, 100, 10), 90..100);
        assert_eq!(viewport(None, 4, 10), 0..4);
        assert_eq!(viewport(None, 0, 10), 0..0);
        assert_eq!(viewport(Some(50), 100, 10), 45..55);
        assert_eq!(viewport(Some(2), 100, 10), 0..10);
        assert_eq!(viewport(Some(99), 100, 10), 90..100);
        assert_eq!(viewport(Some(500), 100, 10), 90..100);
        assert_eq!(viewport(Some(1), 3, 10), 0..3);
    }

    #[test]
    fn history_stamp_is_the_utc_date_and_time_padded() {
        assert_eq!(
            stamp(datetime!(2026-01-02 03:04:05 UTC)),
            "2026-01-02 03:04:05"
        );
        assert_eq!(
            stamp(datetime!(2026-09-23 10:30:05 +2)),
            "2026-09-23 08:30:05"
        );
        assert_eq!(
            stamp(datetime!(2026-12-31 23:59:59 -5)),
            "2027-01-01 04:59:59"
        );
    }

    #[test]
    fn history_rows_show_timestamp_task_kind_and_description() {
        let mut journal = scratch();
        for event in [queued(1), attempt(1, 1), gate(1, false)] {
            journal.add(&event);
        }
        let mut app = loaded((100, 24), &journal.journal, 1);
        // The journal stamped the events now; pin them to a known time.
        if let Some(page) = app.history.page.as_mut() {
            for row in &mut page.rows {
                if let Row::Event(entry) = row {
                    entry.ts = datetime!(2026-09-23 10:30:05 UTC);
                }
            }
        }
        let lines = lines_of(&app);
        assert!(
            lines.iter().any(|l| l.contains(
                "2026-09-23 10:30:05 T1    TaskQueued        task queued: Task number 1"
            )),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("T1    AttemptStarted    attempt started: protocol=tdd"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("T1    GateFinished      gate finished: Verify failed"))
        );
    }

    #[test]
    fn history_groups_are_headed_by_task_and_attempt_and_a_retry_is_a_remediation() {
        let mut journal = scratch();
        for event in [queued(1), attempt(1, 1), failed(1), retry(1, 2)] {
            journal.add(&event);
        }
        let app = loaded((100, 24), &journal.journal, 1);
        let lines = lines_of(&app);
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} in {lines:#?}"))
        };
        let before = at("── task 1 · before any attempt");
        let first = at("── task 1 · attempt 1");
        let second = at("── task 1 · attempt 2 (remediation)");
        assert!(before < at("TaskQueued"));
        assert!(at("TaskQueued") < first);
        assert!(first < at("AttemptStarted"));
        assert!(at("AttemptStarted") < at("TaskFailed"));
        assert!(at("TaskFailed") < second);
        assert!(second < at("RetryStarted"));
        assert!(!lines.iter().any(|l| l.contains("attempt 1 (remediation)")));
    }

    #[test]
    fn history_the_task_timeline_omits_the_task_column_and_names_only_the_attempt() {
        let mut journal = scratch();
        for event in [queued(1), attempt(1, 1), queued(2)] {
            journal.add(&event);
        }
        let mut app = with_tasks(app_on_history((100, 24)), 2);
        app.history.task_scope = true;
        backfill(&mut app, &journal.journal).expect("backfill");
        let lines = lines_of(&app);
        assert!(
            lines.iter().any(|l| l.contains("── attempt 1")),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l.contains("── before any attempt")));
        assert!(!lines.iter().any(|l| l.contains("── task 1")));
        assert!(!lines.iter().any(|l| l.contains("T1 ")));
        assert!(!lines.iter().any(|l| l.contains("Task number 2")));
        assert!(lines.iter().any(|l| l.contains("History · task 1")));
    }

    #[test]
    fn history_the_heading_says_which_timeline_where_the_view_is_and_whether_it_follows() {
        let (_journal, app) = timeline_app(50, (100, 24));
        assert!(
            lines_of(&app)
                .iter()
                .any(|l| l.starts_with("History · whole queue · line 50 of 50 · following"))
        );
        let scrolled = press(app, KeyCode::Char('k'));
        assert!(
            lines_of(&scrolled)
                .iter()
                .any(|l| l
                    .starts_with("History · whole queue · line 49 of 50 · scrolled (G to follow)")),
            "{:#?}",
            lines_of(&scrolled)
        );
    }

    #[test]
    fn history_the_heading_counts_the_first_row_as_one() {
        let (_journal, app) = timeline_app(50, (100, 24));
        let app = press(app, KeyCode::Char('g'));
        assert!(lines_of(&app).iter().any(|l| l.contains("line 1 of 50")));
    }

    #[test]
    fn history_the_selected_row_is_marked_and_no_other() {
        let (_journal, app) = timeline_app(50, (100, 24));
        let app = press(press(app, KeyCode::Char('k')), KeyCode::Char('k'));
        let marked: Vec<String> = lines_of(&app)
            .into_iter()
            .filter(|l| l.starts_with(MARKER))
            .collect();
        assert_eq!(marked.len(), 1, "{marked:?}");
        assert!(marked[0].contains("task queued: 47"), "{marked:?}");
    }

    #[test]
    fn history_following_marks_and_shows_the_newest_row_at_the_bottom() {
        let (_journal, app) = timeline_app(50, (100, 24));
        let lines = lines_of(&app);
        let last_row = lines
            .iter()
            .rposition(|l| l.contains("task queued: "))
            .expect("rows");
        assert!(lines[last_row].starts_with(MARKER));
        assert!(lines[last_row].contains("task queued: 49"));
        // 20 rows are shown: 30..50.
        assert!(
            lines
                .iter()
                .any(|l| l.trim_end().ends_with("task queued: 30"))
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.trim_end().ends_with("task queued: 29"))
        );
    }

    #[test]
    fn history_a_scrolled_view_shows_the_rows_around_the_selected_one() {
        let (journal, app) = timeline_app(100, (100, 24));
        let mut app = press(app, KeyCode::Char('g'));
        backfill(&mut app, &journal.journal).expect("backfill");
        let lines = lines_of(&app);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(MARKER) && l.contains("task queued: 0"))
        );
        assert!(lines.iter().any(|l| l.contains("task queued: 19")));
        assert!(!lines.iter().any(|l| l.contains("task queued: 21")));
    }

    #[test]
    fn history_scrolling_far_and_reading_shows_rows_beyond_the_first_page() {
        let mut journal = scratch();
        numbered(&mut journal, 3 * PAGE_ROWS);
        let mut app = loaded((100, 24), &journal.journal, 0);
        app = press(app, KeyCode::Char('g'));
        backfill(&mut app, &journal.journal).expect("backfill");
        let lines = lines_of(&app);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(MARKER) && l.trim_end().ends_with("task queued: 0")),
            "{lines:#?}"
        );
        let total = 3 * PAGE_ROWS;
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("line 1 of {total}")))
        );
    }

    #[test]
    fn history_levels_are_coloured() {
        let mut journal = scratch();
        for event in [gate(1, true), failed(1)] {
            journal.add(&event);
        }
        let app = loaded((100, 24), &journal.journal, 1);
        let harness = Harness::from_app(app);
        let buffer = harness.buffer();
        let mut colours = Vec::new();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if row.contains("TaskFailed") {
                colours.push(("failed", buffer[(20, y)].fg));
            }
            if row.contains("GateFinished") {
                colours.push(("gate", buffer[(20, y)].fg));
            }
        }
        assert_eq!(colours, [("gate", Color::Reset), ("failed", Color::Red)]);
    }

    #[test]
    fn history_says_so_before_the_journal_has_been_read() {
        let app = app_on_history((80, 24));
        assert!(text_of(&app).contains(NOT_LOADED));
    }

    #[test]
    fn history_says_so_when_there_are_no_events() {
        let journal = scratch();
        let app = loaded((80, 24), &journal.journal, 0);
        assert!(text_of(&app).contains(EMPTY));
        assert!(!text_of(&app).contains("line 0"));
    }

    #[test]
    fn history_says_so_when_the_task_timeline_has_no_task() {
        let mut app = app_on_history((80, 24));
        app.history.task_scope = true;
        let text = text_of(&app);
        assert!(text.contains(NO_TASK));
        assert!(text.contains("History · no task"));
    }

    #[test]
    fn history_a_page_of_another_timeline_is_not_drawn() {
        let mut journal = scratch();
        numbered(&mut journal, 3);
        let mut app = loaded((80, 24), &journal.journal, 1);
        app = press(app, KeyCode::Char('t'));
        let text = text_of(&app);
        assert!(text.contains(NOT_LOADED), "{text}");
        assert!(!text.contains("task queued: 1"));
    }

    #[test]
    fn history_the_key_bar_names_the_other_timeline() {
        let (_journal, app) = timeline_app(3, (100, 24));
        assert!(text_of(&app).contains("t task timeline"));
        assert!(text_of(&app).contains("j/k scroll"));
        let app = press(app, KeyCode::Char('t'));
        assert!(text_of(&app).contains("t queue timeline"));
    }

    #[test]
    fn history_rows_are_cut_at_the_edge_of_the_pane() {
        let mut journal = scratch();
        journal.add(&event(
            Some(1),
            EventKind::TaskQueued {
                title: "字".repeat(200),
            },
        ));
        let app = loaded((40, 24), &journal.journal, 1);
        for line in lines_of(&app) {
            assert!(crate::text::display_width(&line) <= 40, "{line:?}");
        }
    }

    #[test]
    fn history_tiny_terminals_draw_without_panicking() {
        let (_journal, app) = timeline_app(30, (80, 24));
        for (w, h) in [
            (0, 0),
            (1, 1),
            (3, 2),
            (10, 3),
            (10, 4),
            (80, 1),
            (5, 40),
            (200, 60),
        ] {
            let app = update(app.clone(), AppEvent::Resize(w, h));
            let _ = text_of(&app);
        }
    }

    #[test]
    fn history_a_short_body_drops_the_heading_and_the_key_bar_for_rows() {
        let (_journal, app) = timeline_app(30, (80, 3));
        let lines = lines_of(&app);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("task queued: 28"), "{lines:?}");
        assert!(lines[2].contains("task queued: 29"), "{lines:?}");
    }

    #[test]
    fn history_a_body_of_three_rows_keeps_a_heading_a_row_and_the_key_bar() {
        let (_journal, app) = timeline_app(30, (80, 4));
        let lines = lines_of(&app);
        assert!(lines[1].starts_with("History"), "{lines:?}");
        assert!(lines[2].contains("task queued: 29"), "{lines:?}");
        assert!(lines[3].contains("j/k scroll"), "{lines:?}");
    }

    #[test]
    fn history_is_reached_by_its_number_key_and_shows_its_title() {
        let app = update(
            App::new((80, 24)),
            AppEvent::Key(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE)),
        );
        assert_eq!(app.screen, Screen::History);
        assert!(text_of(&app).starts_with("7 History"));
    }
}

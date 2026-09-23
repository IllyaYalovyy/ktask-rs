//! The logs screen: raw and structured logs, filtered by level and phase,
//! searchable, with jumps between matches and between errors.
//!
//! The screen shows one entry per journal event, described the way the
//! on-disk logger describes it ([`LogLine`]): a level, the attempt, the phase
//! and a message. [`fold`] adds each event as it arrives, so the screen is
//! live and needs no file of its own. An event does not always name its
//! phase, so the view records the phase in effect (from `PhaseEntered` until
//! the task ends or its next attempt starts) and gives it to the events that
//! follow; filtering by phase therefore keeps the agent output and gate
//! results of that phase, not only the line that announced it.
//!
//! Every message is sanitized before it is stored: the journal carries
//! provider output, which is untrusted bytes. A message of several lines is
//! kept as one entry with the breaks shown as `⏎`, and an enormous one is cut,
//! so a row is always one row. The entries are a ring of at most
//! [`LOG_WINDOW`]; the journal keeps the rest.
//!
//! There are two views of the same entries, toggled with `v`. The structured
//! view has a column for each field of the record; the raw view is the message
//! alone, as it was recorded.
//!
//! Filters and search compose. `l` raises the minimum level one step
//! (wrapping from error back to debug) and `p` steps through the phases the
//! log has seen, then back to all. What the filters hide is not listed, not
//! searched and not visited by any key. `/` starts typing a search (`Enter`
//! keeps it, `Esc` abandons the typing, and `Esc` afterwards clears the
//! search); the search is a case-insensitive substring match on the message,
//! so it finds the same entries in either view. `n` and `N` go to the next
//! and previous match and `e` and `E` to the next and previous error, all
//! wrapping at the ends, and `j`, `k`, `g` and `G` move as they do elsewhere.
//!
//! The cursor is the number of an entry, not a row, so it stays on the same
//! entry when a filter changes, and lands on the nearest one still shown when
//! that entry is hidden or has left the ring. With no cursor the view follows
//! the newest entry; `G` returns to that, and any other movement fixes the
//! cursor where it lands.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::LayoutPlan;
use crate::sanitize::sanitize;
use crate::text::truncate_to_width;
use crate::types::Screen;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{AttemptId, Event, EventKind, Level, LogLine, Phase, TaskId};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::VecDeque;
use time::OffsetDateTime;

/// The most entries the screen holds. Older ones are dropped as new ones
/// arrive.
pub const LOG_WINDOW: usize = 8_192;

/// The most characters of a message that are kept; the rest is replaced by `…`.
const MAX_MESSAGE_CHARS: usize = 2_048;

/// The most characters of a search that can be typed.
const MAX_QUERY_CHARS: usize = 200;

/// What stands for a line break inside a message.
const BREAK: &str = " ⏎ ";

/// What separates the entries of the status row.
const SEPARATOR: &str = " · ";

/// What the status row shows for a field that has no value.
const NONE: &str = "-";

const EMPTY: &str = "No log entries";

const FILTERED_OUT: &str = "No entries match the filters";

/// The marker in front of the row the cursor is on.
const MARKER: &str = "> ";

/// The padding of every other row, as wide as [`MARKER`].
const NO_MARKER: &str = "  ";

/// One entry of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    number: u64,
    ts: OffsetDateTime,
    level: Level,
    task: Option<TaskId>,
    attempt: Option<AttemptId>,
    phase: Option<Phase>,
    text: String,
}

impl Entry {
    /// The entry's position in the log over the whole run, from zero. It does
    /// not change when older entries are dropped.
    #[must_use]
    pub fn number(&self) -> u64 {
        self.number
    }

    /// How severe the entry is.
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// The phase the entry belongs to, if it belongs to one.
    #[must_use]
    pub fn phase(&self) -> Option<Phase> {
        self.phase
    }

    /// The message, sanitized and on one line.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The logs screen's own state: the entries, the two filters, which view is
/// showing, where the cursor is and any search being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogView {
    entries: VecDeque<Entry>,
    /// How many entries have been added over the run: the next entry's number.
    added: u64,
    /// The phase in effect for events that do not name one.
    phase: Option<Phase>,
    /// The lowest level shown.
    level: Level,
    /// The only phase shown, if the log is filtered to one.
    only_phase: Option<Phase>,
    structured: bool,
    /// The number of the entry the cursor is on; `None` follows the newest.
    cursor: Option<u64>,
    /// The search as typed so far, while it is being typed.
    draft: Option<String>,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            added: 0,
            phase: None,
            level: Level::Debug,
            only_phase: None,
            structured: true,
            cursor: None,
            draft: None,
        }
    }
}

impl LogView {
    /// Whether a search is being typed. While it is, every key is text.
    #[must_use]
    pub fn is_typing(&self) -> bool {
        self.draft.is_some()
    }

    /// How many entries the screen holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the screen holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The lowest level shown.
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// The only phase shown, if the log is filtered to one.
    #[must_use]
    pub fn only_phase(&self) -> Option<Phase> {
        self.only_phase
    }

    /// Whether the structured view is showing, rather than the raw one.
    #[must_use]
    pub fn is_structured(&self) -> bool {
        self.structured
    }

    /// The entry the cursor is on: the one it was put on if it is still
    /// shown, otherwise the nearest one after it, otherwise the newest.
    #[must_use]
    pub fn selected(&self) -> Option<&Entry> {
        let shown = self.shown();
        let at = self.position(&shown)?;
        shown.get(at).copied()
    }

    /// The entries that pass both filters, oldest first.
    fn shown(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.level >= self.level && self.only_phase.is_none_or(|p| entry.phase == Some(p))
            })
            .collect()
    }

    /// Where the cursor is in `shown`.
    fn position(&self, shown: &[&Entry]) -> Option<usize> {
        let last = shown.len().checked_sub(1)?;
        Some(match self.cursor {
            None => last,
            Some(number) => shown
                .iter()
                .position(|entry| entry.number >= number)
                .unwrap_or(last),
        })
    }

    /// The phases the log has seen, in the order it first saw them.
    fn phases(&self) -> Vec<Phase> {
        let mut seen = Vec::new();
        for phase in self.entries.iter().filter_map(|entry| entry.phase) {
            if !seen.contains(&phase) {
                seen.push(phase);
            }
        }
        seen
    }

    fn push(&mut self, event: &Event) {
        let line = LogLine::of(&event.kind);
        let phase = match event.kind {
            EventKind::AttemptStarted { .. } => None,
            _ => line.phase.or(self.phase),
        };
        while self.entries.len() >= LOG_WINDOW {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            number: self.added,
            ts: event.ts,
            level: line.level,
            task: event.task_id,
            attempt: line.attempt,
            phase,
            text: one_line(&line.message),
        });
        self.added = self.added.saturating_add(1);
        match &event.kind {
            EventKind::AttemptStarted { .. }
            | EventKind::TaskDone { .. }
            | EventKind::TaskFailed { .. }
            | EventKind::TaskCancelled { .. } => self.phase = None,
            EventKind::PhaseEntered { phase, .. } => self.phase = Some(*phase),
            _ => {}
        }
    }
}

/// `message` sanitized, on one line and no longer than [`MAX_MESSAGE_CHARS`].
fn one_line(message: &str) -> String {
    let clean = sanitize(message);
    let joined = clean.trim_end_matches('\n').replace('\n', BREAK);
    match joined.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((end, _)) => format!("{}…", joined.get(..end).unwrap_or_default()),
        None => joined,
    }
}

/// Adds the log entry for `event`.
pub fn fold(app: &mut App, event: &Event) {
    app.logs.push(event);
}

/// The search text the entries are matched against, lowercased, if there is
/// one.
fn needle(app: &App) -> Option<String> {
    app.search
        .as_deref()
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase)
}

fn is_match(entry: &Entry, needle: &str) -> bool {
    entry.text.to_lowercase().contains(needle)
}

fn is_error(entry: &Entry) -> bool {
    entry.level == Level::Error
}

/// Moves the cursor to the first entry that passes `hit`, looking forward or
/// backward from the cursor and wrapping round the shown entries. With
/// `inclusive` the entry under the cursor is looked at first when going
/// forward. Nothing moves when no shown entry passes.
fn seek(app: &mut App, forward: bool, inclusive: bool, hit: impl Fn(&Entry) -> bool) {
    let shown = app.logs.shown();
    let len = shown.len();
    let Some(from) = app.logs.position(&shown) else {
        return;
    };
    let found = (0..len)
        .map(|step| {
            if forward {
                (from + usize::from(!inclusive) + step) % len
            } else {
                (from + 2 * len - 1 - step) % len
            }
        })
        .filter_map(|at| shown.get(at).copied())
        .find(|entry| hit(entry))
        .map(Entry::number);
    if let Some(number) = found {
        app.logs.cursor = Some(number);
    }
}

/// Moves the cursor by `rows` (negative is up) within the shown entries,
/// stopping at either end.
fn step(app: &mut App, rows: isize) {
    let shown = app.logs.shown();
    let Some(from) = app.logs.position(&shown) else {
        return;
    };
    let to = from.saturating_add_signed(rows).min(shown.len() - 1);
    app.logs.cursor = shown.get(to).map(|entry| entry.number);
}

fn cycle_level(level: Level) -> Level {
    match level {
        Level::Debug => Level::Info,
        Level::Info => Level::Warn,
        Level::Warn => Level::Error,
        Level::Error => Level::Debug,
    }
}

/// The phase filter after `current`: the first phase the log has seen, then
/// each next one, then none.
fn cycle_phase(phases: &[Phase], current: Option<Phase>) -> Option<Phase> {
    match current {
        None => phases.first().copied(),
        Some(now) => {
            let at = phases.iter().position(|p| *p == now)?;
            phases.get(at + 1).copied()
        }
    }
}

/// Starts a fresh search draft.
fn start_typing(app: &mut App) {
    app.logs.draft = Some(String::new());
}

/// Ends the typing: `Enter` keeps what was typed as the search (an empty one
/// clears it) and goes to the first match at or after the cursor.
fn finish_typing(app: &mut App) {
    let Some(query) = app.logs.draft.take() else {
        return;
    };
    app.search = Some(query).filter(|query| !query.is_empty());
    if let Some(needle) = needle(app) {
        seek(app, true, true, |entry| is_match(entry, &needle));
    }
}

/// Takes `key` as text for the search being typed, if one is. Returns
/// whether it did, in which case no other part of the interface sees the key:
/// `q`, digits and `?` are letters of the search, not commands.
pub fn capture(app: &mut App, key: &KeyEvent) -> bool {
    if app.screen != Screen::Logs || app.overlay.is_some() {
        return false;
    }
    let Some(draft) = app.logs.draft.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc => app.logs.draft = None,
        KeyCode::Enter => finish_typing(app),
        KeyCode::Backspace => {
            draft.pop();
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && draft.chars().count() < MAX_QUERY_CHARS =>
        {
            draft.push(c);
        }
        _ => {}
    }
    true
}

/// Handles the logs screen's keys. Nothing happens on another screen, under
/// an overlay, or while a search is being typed (see [`capture`]).
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if app.screen != Screen::Logs || app.overlay.is_some() || app.logs.is_typing() {
        return;
    }
    let Some(action) = lookup(app.screen, key).map(|binding| binding.action) else {
        return;
    };
    match action {
        KeyAction::MoveDown => step(app, 1),
        KeyAction::MoveUp => step(app, -1),
        KeyAction::First => step(app, isize::MIN),
        KeyAction::Last => app.logs.cursor = None,
        KeyAction::Search => start_typing(app),
        KeyAction::Back => app.search = None,
        KeyAction::ToggleView => app.logs.structured = !app.logs.structured,
        KeyAction::CycleLevel => app.logs.level = cycle_level(app.logs.level),
        KeyAction::CyclePhase => {
            app.logs.only_phase = cycle_phase(&app.logs.phases(), app.logs.only_phase);
        }
        KeyAction::NextMatch | KeyAction::PrevMatch => {
            if let Some(needle) = needle(app) {
                let forward = action == KeyAction::NextMatch;
                seek(app, forward, false, |entry| is_match(entry, &needle));
            }
        }
        KeyAction::NextError => seek(app, true, false, is_error),
        KeyAction::PrevError => seek(app, false, false, is_error),
        _ => {}
    }
}

/// The name of a level in the status row.
fn level_name(level: Level) -> &'static str {
    match level {
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

/// The three-letter tag of a level in the structured view.
fn level_tag(level: Level) -> &'static str {
    match level {
        Level::Debug => "DBG",
        Level::Info => "INF",
        Level::Warn => "WRN",
        Level::Error => "ERR",
    }
}

fn level_style(level: Level) -> Style {
    match level {
        Level::Debug => Style::new().add_modifier(Modifier::DIM),
        Level::Info => Style::new(),
        Level::Warn => Style::new().fg(Color::Yellow),
        Level::Error => Style::new().fg(Color::Red),
    }
}

/// The name of a phase in the interface.
fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Goal => "Goal",
        Phase::Scope => "Scope",
        Phase::AcceptanceTests => "AcceptanceTests",
        Phase::Implement => "Implement",
        Phase::Red => "Red",
        Phase::Green => "Green",
        Phase::Refactor => "Refactor",
        Phase::Review => "Review",
        Phase::Harden => "Harden",
        Phase::DoneCheck => "DoneCheck",
        Phase::Verify => "Verify",
        Phase::Publish => "Publish",
    }
}

/// The status row: the view, the filters, the search and where the cursor is.
fn status(app: &App) -> String {
    let logs = &app.logs;
    let shown = logs.shown();
    let view = if logs.structured { "structured" } else { "raw" };
    let phase = logs.only_phase.map_or(NONE, phase_name);
    let search = match (&logs.draft, needle(app)) {
        (Some(draft), _) => format!("/{draft}_"),
        (None, None) => NONE.to_owned(),
        (None, Some(needle)) => {
            let query = app.search.as_deref().unwrap_or_default();
            let hits: Vec<bool> = shown.iter().map(|e| is_match(e, &needle)).collect();
            let total = hits.iter().filter(|hit| **hit).count();
            let at = logs.position(&shown);
            let here = at.and_then(|at| hits.get(at).copied().filter(|hit| *hit).map(|_| at));
            match (total, here) {
                (0, _) => format!("/{query} no matches"),
                (_, None) => format!("/{query} -/{total}"),
                (_, Some(at)) => {
                    let ordinal = hits.iter().take(at + 1).filter(|hit| **hit).count();
                    format!("/{query} {ordinal}/{total}")
                }
            }
        }
    };
    let place = logs.position(&shown).map_or(0, |at| at + 1);
    let follow = if logs.cursor.is_none() {
        " following"
    } else {
        ""
    };
    format!(
        "{view}{SEPARATOR}level ≥ {}{SEPARATOR}phase {phase}{SEPARATOR}search {search}{SEPARATOR}{place}/{}{follow}",
        level_name(logs.level),
        shown.len(),
    )
}

/// The width of the phase column: that of the longest phase among `shown`,
/// none if no entry has one.
fn phase_width(shown: &[&Entry]) -> usize {
    shown
        .iter()
        .filter_map(|entry| entry.phase)
        .map(|phase| phase_name(phase).len())
        .max()
        .unwrap_or(0)
}

/// The text of a row before it is cut to the pane's width.
fn row_text(entry: &Entry, structured: bool, phase_width: usize) -> String {
    if !structured {
        return entry.text.clone();
    }
    let ts = entry.ts;
    let task = entry
        .task
        .map_or_else(|| NONE.to_owned(), |id| format!("t{id}"));
    let attempt = entry
        .attempt
        .map_or_else(|| NONE.to_owned(), |id| format!("a{id}"));
    let phase = if phase_width > 0 {
        let phase = entry.phase.map_or(NONE, phase_name);
        format!("{phase:<phase_width$} ")
    } else {
        String::new()
    };
    format!(
        "{:02}:{:02}:{:02} {} {task:>4} {attempt:>3} {phase}{}",
        ts.hour(),
        ts.minute(),
        ts.second(),
        level_tag(entry.level),
        entry.text,
    )
}

/// `text` as spans, with each case-insensitive occurrence of `needle`
/// highlighted. When lowercasing changes the text's length the offsets no
/// longer line up with it, and nothing is highlighted.
fn highlighted(text: &str, needle: Option<&str>, base: Style) -> Vec<Span<'static>> {
    let lower = text.to_lowercase();
    let Some(needle) = needle.filter(|n| !n.is_empty() && lower.len() == text.len()) else {
        return vec![Span::styled(text.to_owned(), base)];
    };
    let hit = base.fg(Color::Black).bg(Color::Yellow);
    let mut spans = Vec::new();
    let mut from = 0;
    for (start, found) in lower.match_indices(needle) {
        let end = start + found.len();
        let (Some(before), Some(matched)) = (text.get(from..start), text.get(start..end)) else {
            return vec![Span::styled(text.to_owned(), base)];
        };
        spans.push(Span::styled(before.to_owned(), base));
        spans.push(Span::styled(matched.to_owned(), hit));
        from = end;
    }
    spans.push(Span::styled(
        text.get(from..).unwrap_or_default().to_owned(),
        base,
    ));
    spans
}

/// The first row of the window of `rows` rows over `len` entries that keeps
/// the entry at `at` in the middle, or as near as the ends allow.
fn window_start(len: usize, at: usize, rows: usize) -> usize {
    at.saturating_sub(rows / 2).min(len.saturating_sub(rows))
}

/// Draws the logs into the body of `plan`: the status row, then the entries
/// around the cursor.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let [status_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(body);
    let text = truncate_to_width(&status(app), width);
    let style = Style::new().add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(Line::styled(text, style)), status_area);
    if list_area.is_empty() {
        return;
    }
    let logs = &app.logs;
    let shown = logs.shown();
    let lines: Vec<Line<'_>> = match logs.position(&shown) {
        None => {
            let note = if logs.is_empty() { EMPTY } else { FILTERED_OUT };
            vec![Line::styled(
                truncate_to_width(note, width),
                Style::new().add_modifier(Modifier::DIM),
            )]
        }
        Some(at) => {
            let rows = usize::from(list_area.height);
            let start = window_start(shown.len(), at, rows);
            let columns = phase_width(&shown);
            let needle = needle(app);
            shown
                .iter()
                .enumerate()
                .skip(start)
                .take(rows)
                .map(|(index, entry)| {
                    let selected = index == at;
                    let marker = if selected { MARKER } else { NO_MARKER };
                    let room = width.saturating_sub(marker.len());
                    let row = truncate_to_width(&row_text(entry, logs.structured, columns), room);
                    let mut spans = vec![Span::raw(marker)];
                    spans.extend(highlighted(
                        &row,
                        needle.as_deref(),
                        level_style(entry.level),
                    ));
                    let line = Line::from(spans);
                    if selected {
                        line.style(Style::new().add_modifier(Modifier::REVERSED))
                    } else {
                        line
                    }
                })
                .collect()
        }
    };
    frame.render_widget(Paragraph::new(lines), list_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use ktask_core::{EventSeq, FailureClass, GateKind, GateResult, PauseReason, Stream};

    fn event(kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(3)),
            kind,
        }
    }

    fn output(text: &str) -> EventKind {
        EventKind::AgentOutput {
            attempt: AttemptId::new(1),
            stream: Stream::Stdout,
            text: text.into(),
        }
    }

    fn phase(phase: Phase) -> EventKind {
        EventKind::PhaseEntered {
            attempt: AttemptId::new(1),
            phase,
        }
    }

    fn failed_gate() -> EventKind {
        EventKind::GateFinished {
            result: GateResult {
                kind: GateKind::Verify,
                passed: false,
                exit_code: Some(101),
                signal: None,
                duration_ms: 5,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            },
        }
    }

    /// The fixture. The numbers are the entries' positions, and the errors
    /// are at 4, 8 and 11:
    ///
    /// ```text
    ///  0 info  task queued            -
    ///  1 info  attempt started        -
    ///  2 info  phase entered: Red     Red
    ///  3 debug compiling foo          Red
    ///  4 ERROR gate failed            Red
    ///  5 debug error: foo not found   Red
    ///  6 info  phase entered: Green   Green
    ///  7 debug compiling bar          Green
    ///  8 ERROR verify failed          Green
    ///  9 warn  paused                 Green
    /// 10 debug error again            Green
    /// 11 ERROR task failed            Green
    /// ```
    fn fixture() -> Vec<EventKind> {
        vec![
            EventKind::TaskQueued {
                title: "Add logs".into(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "tdd".into(),
                pid: 7,
                base_sha: "abc".into(),
            },
            phase(Phase::Red),
            output("compiling foo"),
            failed_gate(),
            output("error: foo not found"),
            phase(Phase::Green),
            output("compiling bar"),
            EventKind::VerifyFailed {
                attempt: AttemptId::new(1),
                class: FailureClass::VerificationFailure,
                detail: "error in bar".into(),
            },
            EventKind::Paused {
                reason: PauseReason::Input,
            },
            output("error again"),
            EventKind::TaskFailed {
                class: FailureClass::VerificationFailure,
                detail: "gave up".into(),
            },
        ]
    }

    const ERRORS: [u64; 3] = [4, 8, 11];

    fn logs_app(size: (u16, u16)) -> App {
        let mut app = App::new(size);
        app.screen = Screen::Logs;
        for kind in fixture() {
            fold(&mut app, &event(kind));
        }
        app
    }

    fn harness() -> Harness {
        Harness::from_app(logs_app((100, 30)))
    }

    fn press(harness: &mut Harness, code: KeyCode) {
        harness.send(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn type_text(harness: &mut Harness, text: &str) {
        for c in text.chars() {
            harness.key(c);
        }
    }

    /// Searches for `query`: `/`, the text, `Enter`.
    fn search(harness: &mut Harness, query: &str) {
        harness.key('/');
        type_text(harness, query);
        press(harness, KeyCode::Enter);
    }

    /// The number of the entry the cursor is on.
    fn at(harness: &Harness) -> u64 {
        harness
            .app()
            .logs
            .selected()
            .expect("an entry is selected")
            .number()
    }

    /// The numbers the given key visits, one press at a time.
    fn visits(harness: &mut Harness, key: char, presses: usize) -> Vec<u64> {
        (0..presses)
            .map(|_| {
                harness.key(key);
                at(harness)
            })
            .collect()
    }

    fn rows(harness: &Harness) -> Vec<String> {
        harness.text().lines().map(str::to_owned).collect()
    }

    fn row_of(harness: &Harness, needle: &str) -> Option<String> {
        rows(harness).into_iter().find(|row| row.contains(needle))
    }

    #[test]
    fn logs_holds_one_entry_per_event_in_order_with_the_levels_of_the_logger() {
        let app = logs_app((100, 30));
        let levels: Vec<Level> = app.logs.entries.iter().map(Entry::level).collect();
        let (debug, info, warn, error) = (Level::Debug, Level::Info, Level::Warn, Level::Error);
        assert_eq!(
            levels,
            [
                info, info, info, debug, error, debug, info, debug, error, warn, debug, error
            ]
        );
        let numbers: Vec<u64> = app.logs.entries.iter().map(Entry::number).collect();
        assert_eq!(numbers, (0..12).collect::<Vec<u64>>());
    }

    #[test]
    fn logs_events_carry_the_phase_in_effect_until_the_task_ends() {
        let app = logs_app((100, 30));
        let phases: Vec<Option<Phase>> = app.logs.entries.iter().map(Entry::phase).collect();
        let (red, green) = (Some(Phase::Red), Some(Phase::Green));
        assert_eq!(
            phases,
            [
                None, None, red, red, red, red, green, green, green, green, green, green
            ]
        );
    }

    #[test]
    fn logs_a_new_attempt_starts_with_no_phase() {
        let mut app = logs_app((100, 30));
        app.logs.push(&event(fixture().swap_remove(1)));
        app.logs.push(&event(output("fresh")));
        let last = app.logs.entries.back().expect("entry");
        assert_eq!(last.phase(), None);
        assert_eq!(last.text(), "[Stdout] fresh");
    }

    #[test]
    fn logs_keep_the_phase_an_event_names_over_the_one_in_effect() {
        let mut app = logs_app((100, 30));
        app.logs.push(&event(EventKind::Interrupted {
            phase: Phase::Verify,
        }));
        assert_eq!(
            app.logs.entries.back().map(Entry::phase),
            Some(Some(Phase::Verify))
        );
        app.logs.push(&event(output("after")));
        assert_eq!(app.logs.entries.back().map(Entry::phase), Some(None));
    }

    #[test]
    fn logs_start_on_the_newest_entry_following() {
        let harness = harness();
        assert_eq!(at(&harness), 11);
        assert!(rows(&harness)[1].contains("following"));
    }

    #[test]
    fn logs_e_visits_the_errors_in_order_and_wraps() {
        let mut harness = harness();
        harness.key('g');
        assert_eq!(at(&harness), 0);
        assert_eq!(visits(&mut harness, 'e', 4), [4, 8, 11, 4]);
    }

    #[test]
    fn logs_capital_e_visits_the_errors_backwards_and_wraps() {
        let mut harness = harness();
        assert_eq!(at(&harness), 11);
        assert_eq!(visits(&mut harness, 'E', 4), [8, 4, 11, 8]);
    }

    #[test]
    fn logs_e_does_not_stop_on_entries_that_are_not_errors() {
        let mut harness = harness();
        harness.key('g');
        for expected in ERRORS {
            harness.key('e');
            let entry = harness.app().logs.selected().expect("selected");
            assert_eq!((entry.number(), entry.level()), (expected, Level::Error));
        }
    }

    #[test]
    fn logs_search_goes_to_the_first_match_at_or_after_the_cursor() {
        let mut harness = harness();
        harness.key('g');
        search(&mut harness, "error");
        assert_eq!(harness.app().search.as_deref(), Some("error"));
        assert_eq!(at(&harness), 5);
        assert!(!harness.app().logs.is_typing());
    }

    #[test]
    fn logs_search_keeps_the_cursor_when_it_is_already_on_a_match() {
        let mut harness = harness();
        harness.key('g');
        for _ in 0..3 {
            harness.key('e');
        }
        assert_eq!(at(&harness), 11);
        search(&mut harness, "task failed");
        assert_eq!(at(&harness), 11);
    }

    #[test]
    fn logs_n_visits_the_matches_in_order_and_wraps() {
        let mut harness = harness();
        harness.key('g');
        search(&mut harness, "error");
        // Matches: 5 ("error: foo"), 8 ("error in bar"), 10 ("error again").
        assert_eq!(visits(&mut harness, 'n', 4), [8, 10, 5, 8]);
    }

    #[test]
    fn logs_capital_n_visits_the_matches_backwards_and_wraps() {
        let mut harness = harness();
        harness.key('g');
        search(&mut harness, "error");
        assert_eq!(at(&harness), 5);
        assert_eq!(visits(&mut harness, 'N', 4), [10, 8, 5, 10]);
    }

    #[test]
    fn logs_search_is_case_insensitive() {
        let mut harness = harness();
        harness.key('g');
        search(&mut harness, "COMPILING");
        assert_eq!(at(&harness), 3);
        assert_eq!(visits(&mut harness, 'n', 2), [7, 3]);
    }

    #[test]
    fn logs_n_with_no_search_moves_nothing() {
        let mut harness = harness();
        harness.key('g');
        let before = harness.app().clone();
        harness.key('n');
        harness.key('N');
        assert_eq!(harness.app(), &before);
    }

    #[test]
    fn logs_n_with_a_search_that_matches_nothing_moves_nothing() {
        let mut harness = harness();
        harness.key('g');
        search(&mut harness, "zebra");
        assert_eq!(at(&harness), 0);
        harness.key('n');
        assert_eq!(at(&harness), 0);
        assert!(rows(&harness)[1].contains("/zebra no matches"));
    }

    #[test]
    fn logs_level_filter_hides_entries_below_it_and_e_and_search_skip_them() {
        let mut harness = harness();
        harness.key('l');
        harness.key('l');
        assert_eq!(harness.app().logs.level(), Level::Warn);
        // Warn and above: 4, 8, 9 and 11.
        harness.key('g');
        assert_eq!(at(&harness), 4);
        assert_eq!(visits(&mut harness, 'j', 4), [8, 9, 11, 11]);
        // "error" matches only debug entries besides 8, which is shown.
        harness.key('g');
        search(&mut harness, "error");
        assert_eq!(at(&harness), 8);
        assert_eq!(visits(&mut harness, 'n', 2), [8, 8]);
    }

    #[test]
    fn logs_level_cycles_debug_info_warn_error_and_back() {
        let mut harness = harness();
        let mut seen = vec![harness.app().logs.level()];
        for _ in 0..4 {
            harness.key('l');
            seen.push(harness.app().logs.level());
        }
        assert_eq!(
            seen,
            [
                Level::Debug,
                Level::Info,
                Level::Warn,
                Level::Error,
                Level::Debug
            ]
        );
    }

    #[test]
    fn logs_phase_filter_keeps_only_that_phase_and_e_stays_inside_it() {
        let mut harness = harness();
        harness.key('p');
        assert_eq!(harness.app().logs.only_phase(), Some(Phase::Red));
        harness.key('g');
        assert_eq!(at(&harness), 2);
        // Red holds entries 2..=5 and one error, 4.
        assert_eq!(visits(&mut harness, 'e', 2), [4, 4]);
        harness.key('G');
        assert_eq!(at(&harness), 5);
        harness.key('p');
        assert_eq!(harness.app().logs.only_phase(), Some(Phase::Green));
        harness.key('g');
        assert_eq!(at(&harness), 6);
        assert_eq!(visits(&mut harness, 'e', 3), [8, 11, 8]);
    }

    #[test]
    fn logs_phase_filter_cycles_through_the_phases_seen_and_then_off() {
        let mut harness = harness();
        let mut seen = Vec::new();
        for _ in 0..4 {
            harness.key('p');
            seen.push(harness.app().logs.only_phase());
        }
        assert_eq!(
            seen,
            [Some(Phase::Red), Some(Phase::Green), None, Some(Phase::Red)]
        );
    }

    #[test]
    fn logs_phase_filter_with_no_phase_seen_stays_off() {
        let mut app = App::new((100, 30));
        app.screen = Screen::Logs;
        fold(&mut app, &event(output("hello")));
        let mut harness = Harness::from_app(app);
        harness.key('p');
        assert_eq!(harness.app().logs.only_phase(), None);
    }

    #[test]
    fn logs_filters_and_search_compose_search_looks_only_at_what_is_shown() {
        let mut harness = harness();
        // Green only: "compiling" matches 3 and 7, of which only 7 is shown.
        harness.key('p');
        harness.key('p');
        harness.key('g');
        search(&mut harness, "compiling");
        assert_eq!(at(&harness), 7);
        assert_eq!(visits(&mut harness, 'n', 2), [7, 7]);
        // Add a level filter that hides 7 too: nothing is left to find.
        harness.key('l');
        harness.key('l');
        harness.key('g');
        let before = at(&harness);
        harness.key('n');
        assert_eq!(at(&harness), before);
        assert!(rows(&harness)[1].contains("no matches"));
    }

    #[test]
    fn logs_all_three_filters_compose_level_phase_and_search() {
        let mut harness = harness();
        harness.key('p'); // Red
        harness.key('l'); // ≥ info
        harness.key('l'); // ≥ warn
        harness.key('g');
        assert_eq!(at(&harness), 4);
        search(&mut harness, "gate");
        assert_eq!(at(&harness), 4);
        assert!(rows(&harness)[1].contains("/gate 1/1"));
        harness.key('p'); // Green: the gate entry is out of it
        assert!(rows(&harness)[1].contains("/gate no matches"));
    }

    #[test]
    fn logs_cursor_stays_on_its_entry_when_a_filter_changes() {
        let mut harness = harness();
        harness.key('g');
        for _ in 0..2 {
            harness.key('e');
        }
        assert_eq!(at(&harness), 8);
        harness.key('l');
        harness.key('l');
        assert_eq!(at(&harness), 8);
    }

    #[test]
    fn logs_cursor_moves_to_the_next_shown_entry_when_its_own_is_hidden() {
        let mut harness = harness();
        harness.key('g');
        assert_eq!(visits(&mut harness, 'j', 5), [1, 2, 3, 4, 5]);
        // Entry 5 is debug; at error level the next entry shown is 8.
        for _ in 0..3 {
            harness.key('l');
        }
        assert_eq!(harness.app().logs.level(), Level::Error);
        assert_eq!(at(&harness), 8);
    }

    #[test]
    fn logs_cursor_falls_back_to_the_newest_when_nothing_after_it_is_shown() {
        let mut harness = harness();
        harness.key('g');
        for _ in 0..2 {
            harness.key('e');
        }
        harness.key('j');
        harness.key('j');
        harness.key('j');
        assert_eq!(at(&harness), 11);
        harness.key('k');
        assert_eq!(at(&harness), 10);
        // Hide 10 and everything after it that is not an error: 11 remains.
        for _ in 0..3 {
            harness.key('l');
        }
        assert_eq!(at(&harness), 11);
    }

    #[test]
    fn logs_movement_keys_stop_at_both_ends() {
        let mut harness = harness();
        harness.key('g');
        harness.key('k');
        assert_eq!(at(&harness), 0);
        harness.key('G');
        harness.key('j');
        assert_eq!(at(&harness), 11);
        press(&mut harness, KeyCode::Up);
        assert_eq!(at(&harness), 10);
        press(&mut harness, KeyCode::Down);
        assert_eq!(at(&harness), 11);
    }

    #[test]
    fn logs_capital_g_returns_to_following_the_newest() {
        let mut harness = harness();
        harness.key('g');
        assert!(!rows(&harness)[1].contains("following"));
        harness.key('G');
        assert!(rows(&harness)[1].contains("following"));
        assert_eq!(at(&harness), 11);
    }

    #[test]
    fn logs_a_fixed_cursor_stays_put_as_entries_arrive_and_a_following_one_does_not() {
        let mut app = logs_app((100, 30));
        app.logs.cursor = Some(4);
        fold(&mut app, &event(output("later")));
        assert_eq!(app.logs.selected().map(Entry::number), Some(4));
        app.logs.cursor = None;
        fold(&mut app, &event(output("even later")));
        assert_eq!(app.logs.selected().map(Entry::number), Some(13));
    }

    #[test]
    fn logs_typing_takes_every_key_as_text() {
        let mut harness = harness();
        harness.key('/');
        assert!(harness.app().logs.is_typing());
        type_text(&mut harness, "q3?jgenpvl");
        assert_eq!(harness.app().screen, Screen::Logs);
        assert_eq!(harness.app().overlay, None);
        assert_eq!(harness.app().logs.draft.as_deref(), Some("q3?jgenpvl"));
        assert!(harness.app().logs.is_structured());
        assert_eq!(harness.app().logs.level(), Level::Debug);
        assert!(rows(&harness)[1].contains("search /q3?jgenpvl_"));
    }

    #[test]
    fn logs_backspace_removes_the_last_typed_character() {
        let mut harness = harness();
        harness.key('/');
        type_text(&mut harness, "abc");
        press(&mut harness, KeyCode::Backspace);
        assert_eq!(harness.app().logs.draft.as_deref(), Some("ab"));
        press(&mut harness, KeyCode::Backspace);
        press(&mut harness, KeyCode::Backspace);
        press(&mut harness, KeyCode::Backspace);
        assert_eq!(harness.app().logs.draft.as_deref(), Some(""));
    }

    #[test]
    fn logs_escape_while_typing_abandons_the_draft_and_keeps_the_old_search() {
        let mut harness = harness();
        search(&mut harness, "error");
        harness.key('/');
        type_text(&mut harness, "zzz");
        press(&mut harness, KeyCode::Esc);
        assert!(!harness.app().logs.is_typing());
        assert_eq!(harness.app().search.as_deref(), Some("error"));
    }

    #[test]
    fn logs_escape_after_a_search_clears_it() {
        let mut harness = harness();
        search(&mut harness, "error");
        press(&mut harness, KeyCode::Esc);
        assert_eq!(harness.app().search, None);
        assert!(rows(&harness)[1].contains("search -"));
    }

    #[test]
    fn logs_an_empty_search_clears_the_search() {
        let mut harness = harness();
        search(&mut harness, "error");
        harness.key('/');
        press(&mut harness, KeyCode::Enter);
        assert_eq!(harness.app().search, None);
    }

    #[test]
    fn logs_control_and_alt_chords_are_not_text() {
        let mut harness = harness();
        harness.key('/');
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            harness.send(AppEvent::Key(KeyEvent::new(KeyCode::Char('x'), modifiers)));
        }
        assert_eq!(harness.app().logs.draft.as_deref(), Some(""));
    }

    #[test]
    fn logs_a_search_cannot_grow_without_bound() {
        let mut harness = harness();
        harness.key('/');
        for _ in 0..MAX_QUERY_CHARS + 50 {
            harness.key('a');
        }
        let draft = harness.app().logs.draft.clone().expect("typing");
        assert_eq!(draft.chars().count(), MAX_QUERY_CHARS);
    }

    #[test]
    fn logs_typing_is_captured_only_on_the_logs_screen_and_with_no_overlay() {
        let mut app = logs_app((100, 30));
        app.logs.draft = Some(String::new());
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.screen = Screen::Queue;
        assert!(!capture(&mut app, &key));
        app.screen = Screen::Logs;
        app.overlay = Some(crate::types::Overlay::KeyMap);
        assert!(!capture(&mut app, &key));
        app.overlay = None;
        assert!(capture(&mut app, &key));
        assert_eq!(app.logs.draft.as_deref(), Some("a"));
        app.logs.draft = None;
        assert!(!capture(&mut app, &key));
    }

    #[test]
    fn logs_keys_do_nothing_on_other_screens_or_under_an_overlay() {
        for key in ['n', 'N', 'e', 'E', 'v', 'l', 'p', '/'] {
            let mut app = logs_app((100, 30));
            app.screen = Screen::Git;
            let before = app.clone();
            let mut harness = Harness::from_app(app);
            harness.key(key);
            assert_eq!(harness.app(), &before, "{key} on the Git screen");

            let mut app = logs_app((100, 30));
            app.overlay = Some(crate::types::Overlay::KeyMap);
            let before = app.clone();
            let mut harness = Harness::from_app(app);
            harness.key(key);
            assert_eq!(harness.app(), &before, "{key} under the key map");
        }
    }

    #[test]
    fn logs_v_toggles_between_the_structured_and_the_raw_view() {
        let mut harness = harness();
        let structured = row_of(&harness, "gave up").expect("row");
        assert!(structured.contains("ERR"), "{structured}");
        assert!(structured.contains("t3"), "{structured}");
        assert!(structured.contains("Green"), "{structured}");
        assert!(structured.contains("00:00:00"), "{structured}");
        assert!(rows(&harness)[1].starts_with("structured"));
        harness.key('v');
        assert!(!harness.app().logs.is_structured());
        let raw = row_of(&harness, "gave up").expect("row");
        assert_eq!(
            raw.trim_end(),
            "> task failed (VerificationFailure): gave up"
        );
        assert!(rows(&harness)[1].starts_with("raw"));
        harness.key('v');
        assert!(harness.app().logs.is_structured());
    }

    #[test]
    fn logs_the_structured_view_has_a_column_for_each_field_of_the_record() {
        let harness = harness();
        let row = row_of(&harness, "gate finished").expect("row");
        assert_eq!(
            row.trim_end(),
            "  00:00:00 ERR   t3   - Red   gate finished: Verify failed (exit_code=Some(101))"
        );
    }

    #[test]
    fn logs_the_phase_column_is_left_out_when_no_shown_entry_has_a_phase() {
        let mut app = App::new((100, 30));
        app.screen = Screen::Logs;
        fold(&mut app, &event(output("hello")));
        let harness = Harness::from_app(app);
        assert_eq!(
            row_of(&harness, "hello").expect("row").trim_end(),
            "> 00:00:00 DBG   t3  a1 [Stdout] hello"
        );
    }

    #[test]
    fn logs_the_status_row_names_the_view_the_filters_the_search_and_the_place() {
        let mut harness = harness();
        assert_eq!(
            rows(&harness)[1].trim_end(),
            "structured · level ≥ debug · phase - · search - · 12/12 following"
        );
        harness.key('g');
        harness.key('l');
        harness.key('p');
        harness.key('v');
        search(&mut harness, "gate");
        assert_eq!(
            rows(&harness)[1].trim_end(),
            "raw · level ≥ info · phase Red · search /gate 1/1 · 2/2"
        );
    }

    #[test]
    fn logs_the_status_row_counts_matches_from_the_cursor_entry() {
        let mut harness = harness();
        harness.key('g');
        search(&mut harness, "error");
        assert!(rows(&harness)[1].contains("/error 1/3"));
        harness.key('n');
        assert!(rows(&harness)[1].contains("/error 2/3"));
        harness.key('j');
        assert!(rows(&harness)[1].contains("/error -/3"));
    }

    #[test]
    fn logs_matches_are_highlighted_in_the_row() {
        let mut harness = harness();
        search(&mut harness, "again");
        let buffer = harness.buffer();
        let row = rows(&harness)
            .iter()
            .position(|row| row.contains("error again"))
            .expect("row");
        let x = rows(&harness)[row].find("again").expect("column");
        let cell = &buffer[(u16::try_from(x).expect("x"), u16::try_from(row).expect("y"))];
        assert_eq!(cell.bg, Color::Yellow);
        let before = &buffer[(
            u16::try_from(x - 2).expect("x"),
            u16::try_from(row).expect("y"),
        )];
        assert_ne!(before.bg, Color::Yellow);
    }

    #[test]
    fn logs_the_selected_row_is_marked_and_no_other() {
        let mut harness = harness();
        harness.key('g');
        harness.key('j');
        let marked: Vec<String> = rows(&harness)
            .into_iter()
            .filter(|row| row.starts_with('>'))
            .collect();
        assert_eq!(marked.len(), 1);
        assert!(marked[0].contains("attempt started"), "{marked:?}");
    }

    #[test]
    fn logs_the_window_keeps_the_cursor_in_view_however_long_the_log() {
        let mut app = App::new((60, 10));
        app.screen = Screen::Logs;
        for n in 0..200 {
            fold(&mut app, &event(output(&format!("line {n}"))));
        }
        let mut harness = Harness::from_app(app);
        for expected in [199, 100, 0] {
            match expected {
                199 => {}
                100 => {
                    harness.key('g');
                    for _ in 0..100 {
                        harness.key('j');
                    }
                }
                _ => harness.key('g'),
            }
            assert_eq!(at(&harness), expected);
            let marked = rows(&harness)
                .into_iter()
                .find(|row| row.starts_with('>'))
                .expect("the cursor row is drawn");
            assert!(marked.contains(&format!("line {expected}")), "{marked}");
        }
    }

    #[test]
    fn logs_the_window_shows_the_newest_entries_when_following() {
        let mut app = App::new((60, 10));
        app.screen = Screen::Logs;
        for n in 0..50 {
            fold(&mut app, &event(output(&format!("line {n}"))));
        }
        let harness = Harness::from_app(app);
        let shown = rows(&harness);
        assert!(shown[9].contains("line 49"), "{shown:?}");
        assert!(shown[2].contains("line 42"), "{shown:?}");
        assert!(!shown.iter().any(|row| row.contains("line 41")));
    }

    #[test]
    fn logs_entries_are_a_ring_and_the_cursor_moves_on_when_its_entry_leaves() {
        let mut app = App::new((100, 30));
        app.screen = Screen::Logs;
        for n in 0..LOG_WINDOW + 5 {
            fold(&mut app, &event(output(&format!("line {n}"))));
        }
        assert_eq!(app.logs.len(), LOG_WINDOW);
        assert_eq!(app.logs.entries.front().map(Entry::number), Some(5));
        app.logs.cursor = Some(2);
        assert_eq!(app.logs.selected().map(Entry::number), Some(5));
    }

    #[test]
    fn logs_messages_are_sanitized_and_kept_to_one_line() {
        let mut app = App::new((100, 30));
        app.screen = Screen::Logs;
        fold(
            &mut app,
            &event(output("\x1b[31mred\x1b[0m\x1b]0;t\x07 ok\nsecond\n")),
        );
        let entry = app.logs.entries.back().expect("entry");
        assert_eq!(entry.text(), "[Stdout] red ok ⏎ second");
    }

    #[test]
    fn logs_a_huge_message_is_cut_and_says_so() {
        let mut app = App::new((100, 30));
        app.screen = Screen::Logs;
        fold(&mut app, &event(output(&"é".repeat(100_000))));
        let entry = app.logs.entries.back().expect("entry");
        assert_eq!(entry.text().chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(entry.text().ends_with('…'));
        let mut app = App::new((100, 30));
        fold(&mut app, &event(output(&"x".repeat(MAX_MESSAGE_CHARS - 9))));
        let entry = app.logs.entries.back().expect("entry");
        assert_eq!(entry.text().chars().count(), MAX_MESSAGE_CHARS);
        assert!(!entry.text().ends_with('…'));
    }

    #[test]
    fn logs_rows_are_cut_at_a_grapheme_boundary_and_never_wider_than_the_pane() {
        let mut app = App::new((20, 8));
        app.screen = Screen::Logs;
        app.logs.structured = false;
        fold(&mut app, &event(output("日本語日本語日本語日本語日本語")));
        let harness = Harness::from_app(app);
        // The buffer follows each wide character with a blank cell, so the
        // text of the row has a space after every ideograph.
        let row = row_of(&harness, "[Stdout]").expect("row");
        assert_eq!(row.replace(' ', ""), ">[Stdout]日本語日");
        assert_eq!(row.chars().count(), 20);
    }

    #[test]
    fn logs_an_empty_log_says_so_and_a_filter_that_hides_everything_says_that() {
        let mut app = App::new((60, 10));
        app.screen = Screen::Logs;
        let mut harness = Harness::from_app(app);
        assert!(rows(&harness)[2].contains(EMPTY));
        harness.key('e');
        harness.key('n');
        harness.key('j');
        assert_eq!(harness.app().logs.selected(), None);

        let mut harness = harness_with(&[output("x")]);
        for _ in 0..3 {
            harness.key('l');
        }
        assert!(rows(&harness)[2].contains(FILTERED_OUT));
        assert!(rows(&harness)[1].contains("0/0"));
    }

    fn harness_with(kinds: &[EventKind]) -> Harness {
        let mut app = App::new((60, 10));
        app.screen = Screen::Logs;
        for kind in kinds {
            fold(&mut app, &event(kind.clone()));
        }
        Harness::from_app(app)
    }

    #[test]
    fn logs_render_survives_tiny_and_empty_terminals() {
        for size in [(0, 0), (1, 1), (0, 24), (80, 0), (3, 2), (10, 3), (5, 40)] {
            let mut app = logs_app(size);
            app.search = Some("error".into());
            app.logs.draft = Some("x".into());
            let _ = Harness::from_app(app);
        }
    }

    #[test]
    fn logs_the_screen_is_reached_by_its_number_and_shows_under_its_header() {
        let mut harness = Harness::from_app(App {
            logs: logs_app((100, 30)).logs,
            ..App::new((100, 30))
        });
        harness.key('3');
        assert_eq!(rows(&harness)[0].trim_end(), "3 Logs");
        assert!(rows(&harness)[1].starts_with("structured"));
    }

    #[test]
    fn logs_events_arriving_through_update_are_folded_in() {
        let mut harness = Harness::new(100, 30);
        harness.key('3');
        harness.send(AppEvent::Core(event(output("hello"))));
        assert_eq!(harness.app().logs.len(), 1);
        assert!(row_of(&harness, "hello").is_some());
    }

    #[test]
    fn logs_a_pause_is_shown_as_a_warning() {
        let harness = harness();
        let paused = row_of(&harness, "paused").expect("row");
        assert!(paused.contains("WRN"), "{paused}");
    }

    #[test]
    fn logs_accessors_report_the_state_they_are_named_for() {
        let mut app = logs_app((100, 30));
        assert!(!app.logs.is_empty());
        assert_eq!(app.logs.len(), 12);
        assert!(!app.logs.is_typing());
        app.logs.draft = Some(String::new());
        assert!(app.logs.is_typing());
        assert!(LogView::default().is_empty());
    }
}

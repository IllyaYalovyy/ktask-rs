//! The live-run screen: streaming agent output, the running command and gate results.
//!
//! The body of the [`LayoutPlan`] has two parts. Above, up to three rows of
//! status: the task, attempt, phase and elapsed time; the command being run;
//! and the gate results that have landed. Below, the output pane, which shows
//! the newest lines of [`App::output`] and nothing else.
//!
//! Output is untrusted bytes, so it never reaches the pane as it arrived.
//! [`LiveRun::push`] decodes each chunk with a [`Utf8Stream`] (a character
//! split across two chunks is held back, not damaged), runs the text through
//! [`sanitize`], and only then splits it into lines. A chunk that ends
//! mid-line leaves that line open and the next chunk continues it, so output
//! shows the moment it arrives rather than when its line is finished. The
//! journal's `AgentOutput` events carry whole lines and go through the same
//! path. What is stored is therefore always safe to draw.
//!
//! Drawing costs what the pane costs, not what the run has produced. The
//! lines are a ring of at most [`LiveRun::cap`] entries ([`OUTPUT_WINDOW`]
//! unless [`set_cap`] says otherwise), [`visible`] takes the last few of them
//! without walking the rest, and each is cut at the pane's edge on a grapheme
//! boundary before it is drawn, so even a single enormous line costs one
//! row's width.
//!
//! The ring drops its oldest line when it is full, but the journal keeps
//! every line, and the view can be scrolled back over all of them. Lines are
//! numbered from the first the interface saw; [`LiveRun`] counts how many the
//! ring has let go. When the view reaches into those, [`wanted`] names the
//! numbers it needs and [`backfill`] (the shell's part, since [`update`](crate::update)
//! does no I/O) replays the journal's `AgentOutput` events to fetch them into
//! a second, equally bounded page. Until the page arrives the rows it would
//! fill say so. Memory is therefore at most two caps of lines however long
//! the run.
//!
//! Follow mode decides which lines the pane shows. Following (the default),
//! it shows the newest. Any upward scroll (`k`, `Up`, `g`) detaches it: the
//! pane then holds the lines it showed, however much output arrives, until
//! `f` re-attaches. [`App::scroll`] holds how many lines the view is above the
//! newest, and [`fold`] adds each new line to it while detached, so the
//! viewport stays on the same text. The title bar says which mode is on.
//!
//! The elapsed time is measured in journal time: from the event that started
//! the attempt to the latest event of the run. [`update`](crate::update) has
//! no clock, so it does not advance between events.

use crate::app::{App, OUTPUT_WINDOW};
use crate::keys::{KeyAction, lookup};
use crate::layout::{LayoutPlan, layout_for};
use crate::sanitize::{Utf8Stream, sanitize};
use crate::text::{display_width, truncate_to_width};
use crate::types::Screen;
use crossterm::event::KeyEvent;
use ktask_core::{
    AttemptId, Event, EventKind, EventSeq, GateKind, GateResult, Journal, Phase, Result, Stream,
    TaskId,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::VecDeque;
use std::ops::Range;
use std::time::Duration;
use time::OffsetDateTime;

/// The fewest body rows that get the command and gate rows as well as the
/// status row; a shorter body has the status row and gives the rest to output.
const META_MIN_HEIGHT: u16 = 5;

/// The status rows a body of at least [`META_MIN_HEIGHT`] rows has.
const META_ROWS: u16 = 3;

/// What separates the entries of a status row.
const SEPARATOR: &str = " · ";

/// What separates gate results from each other.
const GATE_GAP: &str = "  ";

/// What a status row shows when it has nothing to say.
const NONE: &str = "-";

const NO_RUN: &str = "No run in progress";

const WAITING: &str = "Waiting for output";

/// What a row of output that has left the ring and is not loaded yet shows.
const NOT_LOADED: &str = "… older output, not loaded";

/// What the title bar adds while the pane follows new output.
const FOLLOWING: &str = " · following";

/// What the title bar adds while it does not, with the key that re-attaches.
const DETACHED: &str = " · detached (f to follow)";

/// The outcome of one gate, as the live run lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateLine {
    /// Which gate ran.
    pub kind: GateKind,
    /// Whether it passed.
    pub passed: bool,
    /// The command's exit code, if it exited normally.
    pub exit_code: Option<i32>,
    /// Whether the runner killed it for running too long.
    pub timed_out: bool,
    /// How long it took, in milliseconds.
    pub duration_ms: u64,
}

impl GateLine {
    fn new(result: &GateResult) -> Self {
        Self {
            kind: result.kind,
            passed: result.passed,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            duration_ms: result.duration_ms,
        }
    }

    /// The result in words: `verify passed 1.2s`, `lint failed (exit 101) 0.4s`
    /// or `build timed out 60.0s`.
    #[must_use]
    pub fn describe(&self) -> String {
        let outcome = match (self.passed, self.timed_out, self.exit_code) {
            (true, _, _) => "passed".to_owned(),
            (false, true, _) => "timed out".to_owned(),
            (false, false, Some(code)) => format!("failed (exit {code})"),
            (false, false, None) => "failed".to_owned(),
        };
        let ms = self.duration_ms;
        format!(
            "{} {outcome} {}.{}s",
            gate_name(self.kind),
            ms / 1000,
            ms % 1000 / 100
        )
    }

    fn style(&self) -> Style {
        Style::new().fg(if self.passed {
            Color::Green
        } else {
            Color::Red
        })
    }
}

/// The name a gate goes by in the interface.
fn gate_name(kind: GateKind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// The live run as the interface knows it: which task is running and where it
/// is, the gate results so far, and the decoders that turn output bytes into
/// the lines of [`App::output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun {
    /// The gate results of the current attempt, oldest first.
    pub gates: Vec<GateLine>,
    stdout: Utf8Stream,
    stderr: Utf8Stream,
    /// The stream of the unfinished last line of the output, if there is one.
    open: Option<Stream>,
    task: Option<TaskId>,
    attempt: Option<AttemptId>,
    phase: Option<Phase>,
    command: Option<String>,
    started: Option<OffsetDateTime>,
    last: Option<OffsetDateTime>,
    running: bool,
    /// How many lines have been started in the output, over the whole run.
    /// Unlike the window it never shrinks, so a caller can tell how many new
    /// lines an event brought even when the window was full.
    added: usize,
    /// The most lines the ring holds.
    cap: usize,
    /// How many lines the ring has dropped, which are the lines numbered
    /// below the ring's first.
    evicted: usize,
    /// Dropped lines fetched back from the journal.
    older: Older,
}

/// A run of lines that have left the ring, read back from the journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Older {
    /// The number of the first line.
    start: usize,
    lines: Vec<String>,
}

impl Older {
    fn get(&self, number: usize) -> Option<&str> {
        let at = number.checked_sub(self.start)?;
        self.lines.get(at).map(String::as_str)
    }
}

impl Default for LiveRun {
    fn default() -> Self {
        Self {
            gates: Vec::new(),
            stdout: Utf8Stream::default(),
            stderr: Utf8Stream::default(),
            open: None,
            task: None,
            attempt: None,
            phase: None,
            command: None,
            started: None,
            last: None,
            running: false,
            added: 0,
            cap: OUTPUT_WINDOW,
            evicted: 0,
            older: Older::default(),
        }
    }
}

impl LiveRun {
    /// The most lines the ring holds.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// How many lines the ring has dropped over the run. They are still in
    /// the journal.
    #[must_use]
    pub fn evicted(&self) -> usize {
        self.evicted
    }

    /// Adds a chunk of `stream` output to `output`.
    ///
    /// The chunk is decoded, sanitized and split into lines. Text after the
    /// last line break is left as an open line, shown at once and continued by
    /// the next chunk of the same stream; a chunk of the other stream closes
    /// it first, so the two never run together.
    pub fn push(&mut self, output: &mut VecDeque<String>, stream: Stream, bytes: &[u8]) {
        self.ingest(output, stream, bytes, false);
    }

    /// Folds one journal event in: the run's task, phase, command and gate
    /// results, and any agent output the event carries.
    pub fn apply(&mut self, output: &mut VecDeque<String>, event: &Event) {
        let at = event.ts;
        if self.running {
            self.last = Some(at);
        }
        match &event.kind {
            EventKind::AttemptStarted {
                attempt,
                protocol,
                pid,
                ..
            } => {
                self.flush(output);
                *self = Self {
                    task: event.task_id,
                    attempt: Some(*attempt),
                    command: Some(format!("agent pid {pid}")),
                    started: Some(at),
                    last: Some(at),
                    running: true,
                    added: self.added,
                    cap: self.cap,
                    evicted: self.evicted,
                    older: std::mem::take(&mut self.older),
                    ..Self::default()
                };
                // The output of every attempt shares one pane, so each starts
                // with a line saying whose it is.
                let task = event
                    .task_id
                    .map_or_else(|| "?".to_owned(), |id| id.to_string());
                let marker = format!("== task {task}, attempt {} ({protocol}) ==", attempt.get());
                self.append(output, Stream::Stdout, &marker);
                self.open = None;
            }
            EventKind::PhaseEntered { phase, .. } => self.phase = Some(*phase),
            EventKind::AgentOutput { stream, text, .. } => {
                self.ingest(output, *stream, text.as_bytes(), true);
            }
            EventKind::AttemptFinished { .. } => {
                self.flush(output);
                self.command = None;
            }
            EventKind::GateStarted { gate } => {
                self.command = Some(format!("gate {}", gate_name(*gate)));
            }
            EventKind::GateFinished { result } => {
                self.gates.push(GateLine::new(result));
                self.command = None;
            }
            EventKind::TaskDone { .. }
            | EventKind::TaskFailed { .. }
            | EventKind::TaskCancelled { .. }
            | EventKind::Interrupted { .. } => self.running = false,
            _ => {}
        }
    }

    /// The time from the start of the attempt to the latest event of the run,
    /// if an attempt has started.
    #[must_use]
    pub fn elapsed(&self) -> Option<Duration> {
        let span = self.last? - self.started?;
        Some(Duration::try_from(span).unwrap_or(Duration::ZERO))
    }

    /// Decodes and sanitizes `bytes` and appends the result to `output`.
    ///
    /// With `whole_lines` the chunk is complete lines (a journal event): a
    /// final line break does not start another line, and nothing is left
    /// open. Without it, text after the last break stays open.
    fn ingest(
        &mut self,
        output: &mut VecDeque<String>,
        stream: Stream,
        bytes: &[u8],
        whole_lines: bool,
    ) {
        if whole_lines || self.open.is_some_and(|open| open != stream) {
            self.open = None;
        }
        let decoded = match stream {
            Stream::Stdout => self.stdout.push(bytes),
            Stream::Stderr => self.stderr.push(bytes),
        };
        let clean = sanitize(&decoded);
        let text = if whole_lines {
            clean.strip_suffix('\n').unwrap_or(&clean)
        } else {
            &clean
        };
        let mut segments = text.split('\n').peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_some() || whole_lines {
                self.append(output, stream, segment);
                self.open = None;
            } else if !segment.is_empty() {
                self.append(output, stream, segment);
            }
        }
    }

    /// Ends both streams: a character that was still incomplete becomes
    /// U+FFFD, and the open line, if any, is closed.
    fn flush(&mut self, output: &mut VecDeque<String>) {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let rest = match stream {
                Stream::Stdout => self.stdout.finish(),
                Stream::Stderr => self.stderr.finish(),
            };
            if !rest.is_empty() {
                self.append(output, stream, &rest);
            }
        }
        self.open = None;
    }

    /// Adds `text` to the open line, or to a new line if none is open, and
    /// leaves that line open. The oldest line goes when the ring is full.
    fn append(&mut self, output: &mut VecDeque<String>, stream: Stream, text: &str) {
        if let Some(line) = output.back_mut().filter(|_| self.open.is_some()) {
            line.push_str(text);
        } else {
            while output.len() >= self.cap {
                output.pop_front();
                self.evicted = self.evicted.saturating_add(1);
            }
            output.push_back(text.to_owned());
            self.added = self.added.saturating_add(1);
        }
        self.open = Some(stream);
    }
}

/// The last `rows` lines of `output`, oldest first, without visiting the
/// others.
pub fn visible(output: &VecDeque<String>, rows: usize) -> impl Iterator<Item = &String> {
    scrolled(output, rows, 0)
}

/// The `rows` lines that end `up` lines above the newest, oldest first,
/// without visiting the others. `up` past the point where the oldest line is
/// at the top of the pane is taken as that point.
fn scrolled(output: &VecDeque<String>, rows: usize, up: usize) -> impl Iterator<Item = &String> {
    let up = up.min(output.len().saturating_sub(rows));
    let end = output.len() - up;
    output.range(end.saturating_sub(rows)..end)
}

/// The body of the live screen split into its status rows and output pane.
fn areas(body: Rect) -> [Rect; 2] {
    let meta_rows = if body.height >= META_MIN_HEIGHT {
        META_ROWS
    } else {
        1
    };
    Layout::vertical([Constraint::Length(meta_rows), Constraint::Fill(1)]).areas(body)
}

/// The rows of the output pane.
fn pane_rows(app: &App) -> usize {
    let (width, height) = app.size;
    let [_, pane] = areas(layout_for(Rect::new(0, 0, width, height)).body);
    usize::from(pane.height)
}

/// How many lines the run has produced that the interface knows of: those
/// the ring dropped and those it holds.
fn total(app: &App) -> usize {
    app.live.evicted.saturating_add(app.output.len())
}

/// How far the view can be above the newest line: the lines that do not fit
/// in the pane, dropped or not.
fn max_up(app: &App) -> usize {
    total(app).saturating_sub(pane_rows(app))
}

/// The numbers of the lines the pane shows, `rows` of them, ending `up`
/// lines above the newest.
fn view_span(app: &App, rows: usize, up: usize) -> Range<usize> {
    let all = total(app);
    let end = all - up.min(all.saturating_sub(rows));
    end.saturating_sub(rows)..end
}

/// The line numbered `number`, if the ring or the fetched page has it.
fn line_at(app: &App, number: usize) -> Option<&str> {
    match number.checked_sub(app.live.evicted) {
        Some(at) => app.output.get(at).map(String::as_str),
        None => app.live.older.get(number),
    }
}

/// The lines of output the view needs that have left the ring and are not
/// in the fetched page: a range of line numbers, at most [`LiveRun::cap`]
/// long and placed so that scrolling on in either direction stays inside it
/// for a while. `None` when the ring and the page already cover the view.
#[must_use]
pub fn wanted(app: &App) -> Option<Range<usize>> {
    let rows = pane_rows(app);
    let up = if app.follow { 0 } else { up(app) };
    let span = view_span(app, rows, up);
    let live = &app.live;
    let missing = span.start..span.end.min(live.evicted);
    if missing
        .clone()
        .all(|number| live.older.get(number).is_some())
    {
        return None;
    }
    let first = span.start.saturating_sub(live.cap.saturating_sub(rows) / 2);
    Some(first..first.saturating_add(live.cap).min(live.evicted))
}

/// Sets the ring's cap (at least one line), dropping the oldest lines if it
/// now holds more, and trims the fetched page to match.
pub fn set_cap(app: &mut App, cap: usize) {
    let cap = cap.max(1);
    app.live.cap = cap;
    while app.output.len() > cap {
        app.output.pop_front();
        app.live.evicted = app.live.evicted.saturating_add(1);
    }
    app.live.older.lines.truncate(cap);
}

/// The lines `event` adds to the output, as the interface would store them.
fn event_lines(event: &Event) -> Vec<String> {
    let mut scratch = LiveRun {
        cap: usize::MAX,
        ..LiveRun::default()
    };
    let mut lines = VecDeque::new();
    scratch.apply(&mut lines, event);
    lines.into()
}

/// Reads the lines numbered `range` from `journal`, counting from the first
/// line of its first event.
///
/// The journal is streamed, so memory is the range plus one event; only lines
/// in the range are kept. Numbers match those of an interface that was fed
/// the journal from the start, as [`Attachment`](crate::Attachment) does.
///
/// # Errors
///
/// See [`Journal::for_each_event`].
pub fn read_lines(journal: &Journal, range: Range<usize>) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut number = 0usize;
    journal.for_each_event(EventSeq::new(0), &mut |event| {
        if number < range.end {
            for line in event_lines(&event) {
                if range.contains(&number) {
                    found.push(line);
                }
                number += 1;
            }
        }
        Ok(())
    })?;
    Ok(found)
}

/// Fetches from `journal` the lines [`wanted`] names into the page the view
/// reads them from, replacing the previous page. Returns whether it fetched.
///
/// The shell calls this after a turn that scrolled; it is the one place the
/// live screen does I/O.
///
/// # Errors
///
/// See [`read_lines`]. The page is unchanged on error.
pub fn backfill(app: &mut App, journal: &Journal) -> Result<bool> {
    let Some(range) = wanted(app) else {
        return Ok(false);
    };
    let start = range.start;
    let lines = read_lines(journal, range)?;
    app.live.older = Older { start, lines };
    Ok(true)
}

/// How many lines the view is above the newest.
fn up(app: &App) -> usize {
    app.scroll.get(&Screen::LiveRun).copied().unwrap_or(0)
}

/// What the title bar adds after the screen's name: whether the pane follows.
#[must_use]
pub fn title_suffix(app: &App) -> &'static str {
    if app.follow { FOLLOWING } else { DETACHED }
}

/// Folds one journal event into the run state, keeping a detached view on the
/// lines it was showing: each line the event added moves the view that much
/// further from the newest.
pub fn fold(app: &mut App, event: &Event) {
    let before = app.live.added;
    app.live.apply(&mut app.output, event);
    let grown = app.live.added.saturating_sub(before);
    if !app.follow && grown > 0 {
        let at = up(app).saturating_add(grown).min(max_up(app));
        app.scroll.insert(Screen::LiveRun, at);
    }
}

/// Handles the live screen's keys: `k`, `Up` and `g` scroll up and so detach
/// follow; `j`, `Down` and `G` scroll back down, which does not re-attach it;
/// `f` re-attaches. Nothing happens on another screen or under an overlay.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if app.screen != Screen::LiveRun || app.overlay.is_some() {
        return;
    }
    let scroll = match lookup(app.screen, key).map(|binding| binding.action) {
        Some(KeyAction::MoveUp) => up(app).saturating_add(1),
        Some(KeyAction::First) => usize::MAX,
        Some(KeyAction::MoveDown) => up(app).saturating_sub(1),
        Some(KeyAction::Last) => 0,
        Some(KeyAction::Follow) => {
            app.follow = true;
            app.scroll.remove(&Screen::LiveRun);
            return;
        }
        _ => return,
    };
    if scroll > up(app) {
        app.follow = false;
    }
    app.scroll.insert(Screen::LiveRun, scroll.min(max_up(app)));
}

/// `h:mm:ss`.
fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// The status row: which task is running and where it is.
fn status_line(live: &LiveRun, width: usize) -> Line<'static> {
    let (Some(task), Some(attempt)) = (live.task, live.attempt) else {
        return Line::styled(
            truncate_to_width(NO_RUN, width),
            Style::new().add_modifier(Modifier::DIM),
        );
    };
    let phase = live
        .phase
        .map_or_else(|| NONE.to_owned(), |phase| format!("{phase:?}"));
    let elapsed = live.elapsed().map_or_else(|| NONE.to_owned(), clock);
    let mut text = format!(
        "task {task}{SEPARATOR}attempt {}{SEPARATOR}phase {phase}{SEPARATOR}elapsed {elapsed}",
        attempt.get()
    );
    if !live.running {
        text.push_str(SEPARATOR);
        text.push_str("ended");
    }
    Line::styled(
        truncate_to_width(&text, width),
        Style::new().add_modifier(Modifier::BOLD),
    )
}

/// The command row.
fn command_line(live: &LiveRun, width: usize) -> Line<'static> {
    let text = format!("command: {}", live.command.as_deref().unwrap_or(NONE));
    Line::raw(truncate_to_width(&text, width))
}

/// The gates row: the newest results that fit, oldest first, so the result
/// that just landed is never the one that is cut.
fn gates_line(live: &LiveRun, width: usize) -> Line<'static> {
    const LABEL: &str = "gates: ";
    let room = width.saturating_sub(display_width(LABEL));
    let mut shown: Vec<(String, Style)> = Vec::new();
    let mut used = 0;
    for gate in live.gates.iter().rev() {
        let text = gate.describe();
        let needed = display_width(&text) + if shown.is_empty() { 0 } else { GATE_GAP.len() };
        if used + needed > room {
            break;
        }
        used += needed;
        shown.push((text, gate.style()));
    }
    if shown.is_empty() {
        // Either there are no results, or not even the newest fits.
        let text = live
            .gates
            .last()
            .map_or_else(|| NONE.to_owned(), GateLine::describe);
        let style = live.gates.last().map_or_else(Style::new, GateLine::style);
        return Line::from(vec![
            Span::raw(truncate_to_width(LABEL, width)),
            Span::styled(truncate_to_width(&text, room), style),
        ]);
    }
    let mut spans = vec![Span::raw(LABEL)];
    for (at, (text, style)) in shown.into_iter().rev().enumerate() {
        if at > 0 {
            spans.push(Span::raw(GATE_GAP));
        }
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Draws the live run into the body of `plan`: the status rows, then the
/// lines of output filling the rest: the newest while following, otherwise
/// those the view is scrolled to.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let [meta_area, output_area] = areas(body);
    let meta_rows = meta_area.height;
    let mut meta = vec![
        status_line(&app.live, width),
        command_line(&app.live, width),
        gates_line(&app.live, width),
    ];
    meta.truncate(usize::from(meta_rows));
    frame.render_widget(Paragraph::new(meta), meta_area);
    if output_area.is_empty() {
        return;
    }
    let lines: Vec<Line<'_>> = if app.output.is_empty() && app.live.evicted == 0 {
        vec![Line::styled(
            truncate_to_width(WAITING, width),
            Style::new().add_modifier(Modifier::DIM),
        )]
    } else {
        let up = if app.follow { 0 } else { up(app) };
        let span = view_span(app, usize::from(output_area.height), up);
        span.map(|number| match line_at(app, number) {
            Some(line) => Line::raw(truncate_to_width(line, width)),
            None => Line::styled(
                truncate_to_width(NOT_LOADED, width),
                Style::new().add_modifier(Modifier::DIM),
            ),
        })
        .collect()
    };
    frame.render_widget(Paragraph::new(lines), output_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::Screen;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ktask_core::{EventSeq, GateKind};
    use time::Duration as TimeDuration;

    const T0: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn at(secs: i64, task: Option<u32>, kind: EventKind) -> AppEvent {
        AppEvent::Core(Event {
            seq: EventSeq::new(1),
            ts: T0 + TimeDuration::seconds(secs),
            task_id: task.map(TaskId::new),
            kind,
        })
    }

    fn core(kind: EventKind) -> AppEvent {
        at(0, Some(3), kind)
    }

    fn out(text: &str) -> AppEvent {
        core(EventKind::AgentOutput {
            attempt: AttemptId::new(1),
            stream: Stream::Stdout,
            text: text.into(),
        })
    }

    fn started(pid: u32) -> AppEvent {
        core(EventKind::AttemptStarted {
            attempt: AttemptId::new(2),
            protocol: "tdd".into(),
            pid,
            base_sha: "abc".into(),
        })
    }

    fn phase(phase: Phase) -> AppEvent {
        core(EventKind::PhaseEntered {
            attempt: AttemptId::new(2),
            phase,
        })
    }

    fn gate_started(gate: GateKind) -> AppEvent {
        core(EventKind::GateStarted { gate })
    }

    fn gate_finished(kind: GateKind, passed: bool, exit_code: Option<i32>) -> AppEvent {
        core(EventKind::GateFinished {
            result: GateResult {
                kind,
                passed,
                exit_code,
                signal: None,
                duration_ms: 1_234,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            },
        })
    }

    fn live_harness(w: u16, h: u16) -> Harness {
        let mut harness = Harness::new(w, h);
        harness.send(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        )));
        assert_eq!(harness.app().screen, Screen::LiveRun);
        harness
    }

    /// The screen, one string per row. A wide symbol also covers the cell
    /// after it, so that cell is skipped and each row is `width` columns.
    fn rows(harness: &Harness) -> Vec<String> {
        let buffer = harness.buffer();
        let area = buffer.area;
        (area.top()..area.bottom())
            .map(|y| {
                let mut row = String::new();
                let mut x = area.left();
                while x < area.right() {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += u16::try_from(display_width(symbol).max(1)).unwrap_or(1);
                }
                row
            })
            .collect()
    }

    fn lines(output: &VecDeque<String>) -> Vec<&str> {
        output.iter().map(String::as_str).collect()
    }

    // --- ingest: bytes in, sanitized lines out -------------------------------

    #[test]
    fn live_output_appears_as_each_chunk_is_fed() {
        let mut harness = live_harness(80, 24);
        let mut seen: Vec<String> = Vec::new();
        for chunk in [
            "compiling ktask-core",
            "compiling ktask-tui",
            "finished dev",
        ] {
            harness.send(out(chunk));
            seen.push(chunk.to_owned());
            let screen = harness.text();
            for earlier in &seen {
                assert!(screen.contains(earlier.as_str()), "{earlier} missing");
            }
            assert_eq!(harness.app().output.back().map(String::as_str), Some(chunk));
        }
        assert_eq!(harness.app().output.len(), 3);
    }

    #[test]
    fn live_a_partial_line_shows_at_once_and_is_extended_by_the_next_chunk() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, b"abc");
        assert_eq!(lines(&output), ["abc"]);
        live.push(&mut output, Stream::Stdout, b"de\nwor");
        assert_eq!(lines(&output), ["abcde", "wor"]);
        live.push(&mut output, Stream::Stdout, b"ld\n");
        assert_eq!(lines(&output), ["abcde", "world"]);
        live.push(&mut output, Stream::Stdout, b"next");
        assert_eq!(lines(&output), ["abcde", "world", "next"]);
    }

    #[test]
    fn live_an_empty_chunk_adds_nothing_and_a_blank_line_is_kept() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, b"");
        assert!(output.is_empty());
        live.push(&mut output, Stream::Stdout, b"a\n\nb\n");
        assert_eq!(lines(&output), ["a", "", "b"]);
    }

    #[test]
    fn live_a_character_split_across_chunks_is_decoded_whole() {
        let bytes = "aéb".as_bytes();
        let (head, tail) = bytes.split_at(2);
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, head);
        assert_eq!(lines(&output), ["a"]);
        live.push(&mut output, Stream::Stdout, tail);
        assert_eq!(lines(&output), ["aéb"]);
    }

    #[test]
    fn live_invalid_utf8_becomes_a_replacement_character() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, b"a\xffb\n");
        assert_eq!(lines(&output), ["a\u{fffd}b"]);
    }

    #[test]
    fn live_chunks_are_sanitized_before_they_are_stored() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(
            &mut output,
            Stream::Stdout,
            b"\x1b[31mred\x1b[0m\x1b]0;t\x07 ok\x07\n| a\r/ a\n",
        );
        assert_eq!(lines(&output), ["red ok\u{2426}", "| a", "/ a"]);
    }

    #[test]
    fn live_each_stream_keeps_its_own_undecoded_tail() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, &[0xc3]);
        live.push(&mut output, Stream::Stderr, b"warn\n");
        live.push(&mut output, Stream::Stdout, &[0xa9, b'\n']);
        assert_eq!(lines(&output), ["warn", "é"]);
    }

    #[test]
    fn live_a_line_open_on_one_stream_is_closed_by_the_other() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, b"ab");
        live.push(&mut output, Stream::Stderr, b"cd\n");
        live.push(&mut output, Stream::Stdout, b"ef\n");
        assert_eq!(lines(&output), ["ab", "cd", "ef"]);
    }

    #[test]
    fn live_output_is_windowed_to_the_most_recent_lines() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        for n in 0..(OUTPUT_WINDOW + 10) {
            live.push(
                &mut output,
                Stream::Stdout,
                format!("line {n}\n").as_bytes(),
            );
        }
        assert_eq!(output.len(), OUTPUT_WINDOW);
        assert_eq!(output.front().map(String::as_str), Some("line 10"));
    }

    #[test]
    fn live_an_event_is_whole_lines_even_after_an_open_partial_line() {
        let mut live = LiveRun::default();
        let mut output = VecDeque::new();
        live.push(&mut output, Stream::Stdout, b"partial");
        let event = Event {
            seq: EventSeq::new(1),
            ts: T0,
            task_id: Some(TaskId::new(3)),
            kind: EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "whole".into(),
            },
        };
        live.apply(&mut output, &event);
        assert_eq!(lines(&output), ["partial", "whole"]);
    }

    #[test]
    fn live_finishing_an_attempt_completes_a_held_back_character_and_closes_the_line() {
        let mut harness = live_harness(80, 24);
        let mut app = harness.app().clone();
        app.live.push(&mut app.output, Stream::Stdout, b"tail \xc3");
        let finished = core(EventKind::AttemptFinished {
            attempt: AttemptId::new(2),
            exit_code: 0,
            usage: None,
            session_id: None,
            model_reported: None,
        });
        let mut app = crate::app::update(app, finished);
        assert_eq!(lines(&app.output), ["tail \u{fffd}"]);
        app.live.push(&mut app.output, Stream::Stdout, b"next");
        assert_eq!(lines(&app.output), ["tail \u{fffd}", "next"]);
        harness = Harness::from_app(app);
        assert!(harness.text().contains("next"));
    }

    // --- state folded from journal events -----------------------------------

    #[test]
    fn live_no_run_says_so_and_waits_for_output() {
        let harness = live_harness(80, 24);
        let screen = harness.text();
        assert!(screen.contains("No run in progress"), "{screen}");
        assert!(screen.contains("Waiting for output"), "{screen}");
        assert!(screen.contains("command: -"), "{screen}");
        assert!(screen.contains("gates: -"), "{screen}");
    }

    #[test]
    fn live_a_started_attempt_names_its_task_attempt_and_command_and_marks_the_output() {
        let mut harness = live_harness(80, 24);
        harness.send(started(4242));
        let rows = rows(&harness);
        assert!(rows[1].starts_with("task 3 · attempt 2 · phase - · elapsed 0:00:00"));
        assert!(
            rows[2].starts_with("command: agent pid 4242"),
            "{}",
            rows[2]
        );
        assert!(!harness.text().contains("Waiting for output"));
        assert_eq!(
            harness.app().output.back().map(String::as_str),
            Some("== task 3, attempt 2 (tdd) ==")
        );
    }

    #[test]
    fn live_the_current_phase_follows_phase_events() {
        let mut harness = live_harness(80, 24);
        harness.send(started(1));
        harness.send(phase(Phase::Red));
        assert!(rows(&harness)[1].contains("phase Red"));
        harness.send(phase(Phase::Green));
        let status = rows(&harness)[1].clone();
        assert!(status.contains("phase Green"), "{status}");
        assert!(!status.contains("Red"), "{status}");
    }

    #[test]
    fn live_the_command_is_the_gate_while_it_runs_and_clears_when_it_finishes() {
        let mut harness = live_harness(80, 24);
        harness.send(started(7));
        harness.send(gate_started(GateKind::Verify));
        assert!(rows(&harness)[2].starts_with("command: gate verify"));
        harness.send(gate_finished(GateKind::Verify, true, Some(0)));
        assert!(rows(&harness)[2].starts_with("command: -"));
    }

    #[test]
    fn live_finishing_the_attempt_clears_the_command() {
        let mut harness = live_harness(80, 24);
        harness.send(started(7));
        harness.send(core(EventKind::AttemptFinished {
            attempt: AttemptId::new(2),
            exit_code: 0,
            usage: None,
            session_id: None,
            model_reported: None,
        }));
        assert!(rows(&harness)[2].starts_with("command: -"));
    }

    #[test]
    fn live_gate_results_are_listed_as_they_land() {
        let mut harness = live_harness(80, 24);
        harness.send(started(7));
        harness.send(gate_finished(GateKind::Format, true, Some(0)));
        let gates = rows(&harness)[3].clone();
        assert!(gates.starts_with("gates: format passed 1.2s"), "{gates}");
        harness.send(gate_finished(GateKind::Lint, false, Some(101)));
        let gates = rows(&harness)[3].clone();
        assert!(gates.contains("format passed 1.2s"), "{gates}");
        assert!(gates.contains("lint failed (exit 101) 1.2s"), "{gates}");
    }

    #[test]
    fn live_a_gate_that_timed_out_or_was_killed_says_so() {
        let mut app = App::new((80, 24));
        for (timed_out, signal, exit_code) in [(true, None, None), (false, Some(9), None)] {
            let event = Event {
                seq: EventSeq::new(1),
                ts: T0,
                task_id: Some(TaskId::new(3)),
                kind: EventKind::GateFinished {
                    result: GateResult {
                        kind: GateKind::Build,
                        passed: false,
                        exit_code,
                        signal,
                        duration_ms: 60_000,
                        stdout: String::new(),
                        stderr: String::new(),
                        timed_out,
                    },
                },
            };
            app.live.apply(&mut app.output, &event);
        }
        let described: Vec<String> = app.live.gates.iter().map(GateLine::describe).collect();
        assert_eq!(described, ["build timed out 60.0s", "build failed 60.0s"]);
    }

    #[test]
    fn live_a_new_attempt_starts_with_no_gate_results() {
        let mut harness = live_harness(80, 24);
        harness.send(started(7));
        harness.send(gate_finished(GateKind::Lint, false, Some(1)));
        harness.send(started(8));
        assert!(rows(&harness)[3].starts_with("gates: -"));
    }

    #[test]
    fn live_a_passed_gate_is_green_and_a_failed_one_red() {
        let mut harness = live_harness(80, 24);
        harness.send(started(7));
        harness.send(gate_finished(GateKind::Format, true, Some(0)));
        harness.send(gate_finished(GateKind::Lint, false, Some(1)));
        let buffer = harness.buffer();
        let colour_at = |needle: &str| {
            let row: String = (0..80).map(|x| buffer[(x, 3)].symbol()).collect();
            let x = u16::try_from(row.find(needle).expect("gate shown")).expect("column");
            buffer[(x, 3)].fg
        };
        assert_eq!(colour_at("format"), Color::Green);
        assert_eq!(colour_at("lint"), Color::Red);
    }

    #[test]
    fn live_gates_drop_the_oldest_result_to_keep_the_newest_in_view() {
        let mut harness = live_harness(40, 24);
        harness.send(started(7));
        for kind in [
            GateKind::Format,
            GateKind::Lint,
            GateKind::Build,
            GateKind::Verify,
        ] {
            harness.send(gate_finished(kind, true, Some(0)));
        }
        let gates = rows(&harness)[3].trim_end().to_owned();
        assert!(gates.contains("verify passed 1.2s"), "{gates}");
        assert!(!gates.contains("format"), "{gates}");
        assert!(display_width(&gates) <= 40);
    }

    #[test]
    fn live_gates_that_exactly_fill_the_row_are_all_shown_with_a_gap_between() {
        // "gates: " + "lint passed 1.2s" + two spaces + "verify passed 1.2s".
        let mut harness = live_harness(43, 24);
        harness.send(started(7));
        harness.send(gate_finished(GateKind::Lint, true, Some(0)));
        harness.send(gate_finished(GateKind::Verify, true, Some(0)));
        assert_eq!(
            rows(&harness)[3],
            "gates: lint passed 1.2s  verify passed 1.2s"
        );
        harness.send(AppEvent::Resize(42, 24));
        assert_eq!(rows(&harness)[3].trim_end(), "gates: verify passed 1.2s");
    }

    #[test]
    fn live_a_gate_line_too_narrow_for_even_one_result_is_cut_not_wrapped() {
        let mut harness = live_harness(80, 24);
        harness.send(started(7));
        harness.send(gate_finished(GateKind::Verify, true, Some(0)));
        harness.send(AppEvent::Resize(14, 24));
        let rows = rows(&harness);
        assert_eq!(rows[3].trim_end(), "gates: verify");
        assert!(rows[4].starts_with("== task 3"), "{}", rows[4]);
    }

    #[test]
    fn live_elapsed_time_runs_from_the_attempt_start_to_the_latest_event() {
        let mut harness = live_harness(80, 24);
        harness.send(at(100, Some(3), started_kind()));
        harness.send(at(100 + 3_723, Some(3), phase_kind(Phase::Implement)));
        assert!(rows(&harness)[1].contains("elapsed 1:02:03"));
    }

    #[test]
    fn live_elapsed_time_stops_when_the_task_ends() {
        for end in [
            EventKind::TaskDone { commit: "c".into() },
            EventKind::TaskFailed {
                class: ktask_core::FailureClass::PolicyFailure,
                detail: "d".into(),
            },
            EventKind::TaskCancelled { reason: "r".into() },
            EventKind::Interrupted {
                phase: Phase::Implement,
            },
        ] {
            let mut harness = live_harness(80, 24);
            harness.send(at(0, Some(3), started_kind()));
            harness.send(at(60, Some(3), end));
            harness.send(at(600, Some(4), EventKind::PreflightStarted));
            let status = rows(&harness)[1].clone();
            assert!(status.contains("elapsed 0:01:00"), "{status}");
            assert!(status.contains("ended"), "{status}");
        }
    }

    #[test]
    fn live_a_clock_that_ran_backwards_shows_zero_elapsed() {
        let mut harness = live_harness(80, 24);
        harness.send(at(100, Some(3), started_kind()));
        harness.send(at(50, Some(3), phase_kind(Phase::Red)));
        assert!(rows(&harness)[1].contains("elapsed 0:00:00"));
    }

    fn started_kind() -> EventKind {
        EventKind::AttemptStarted {
            attempt: AttemptId::new(2),
            protocol: "tdd".into(),
            pid: 1,
            base_sha: "abc".into(),
        }
    }

    fn phase_kind(phase: Phase) -> EventKind {
        EventKind::PhaseEntered {
            attempt: AttemptId::new(2),
            phase,
        }
    }

    // --- rendering ------------------------------------------------------------

    #[test]
    fn live_shows_the_newest_lines_when_there_are_more_than_fit() {
        let mut harness = live_harness(80, 24);
        for n in 0..500 {
            harness.send(out(&format!("line {n}")));
        }
        let rows = rows(&harness);
        // Header, three status rows, output, footer.
        assert_eq!(rows.len(), 24);
        assert_eq!(rows[4].trim_end(), "line 481");
        assert_eq!(rows[22].trim_end(), "line 499");
        assert!(!harness.text().contains("line 480"));
    }

    #[test]
    fn live_the_rows_drawn_do_not_depend_on_how_much_output_there_has_been() {
        let mut few = VecDeque::new();
        let mut many = VecDeque::new();
        for n in 0..30 {
            few.push_back(format!("line {n}"));
        }
        for n in 0..OUTPUT_WINDOW {
            many.push_back(format!("line {n}"));
        }
        assert_eq!(visible(&few, 19).count(), 19);
        assert_eq!(visible(&many, 19).count(), 19);
        assert_eq!(
            visible(&many, 19).last().map(String::as_str),
            Some("line 4095")
        );
        assert_eq!(
            visible(&many, 19).next().map(String::as_str),
            Some("line 4077")
        );
        assert_eq!(visible(&few, 100).count(), 30);
        assert_eq!(visible(&few, 0).count(), 0);
    }

    #[test]
    fn live_render_of_a_full_window_shows_the_last_line_and_only_the_last_lines() {
        let mut app = App::new((80, 24));
        app.screen = Screen::LiveRun;
        for n in 0..OUTPUT_WINDOW {
            app.output.push_back(format!("line {n}"));
        }
        let harness = Harness::from_app(app);
        let rows = rows(&harness);
        assert_eq!(rows[22].trim_end(), "line 4095");
        assert_eq!(rows[4].trim_end(), "line 4077");
    }

    #[test]
    fn live_a_line_longer_than_the_pane_is_cut_at_the_edge_and_never_wrapped() {
        let mut harness = live_harness(40, 12);
        harness.send(out(&"x".repeat(100_000)));
        harness.send(out("after"));
        let rows = rows(&harness);
        let at = rows
            .iter()
            .position(|r| r.starts_with("xxxx"))
            .expect("long line");
        assert_eq!(rows[at], "x".repeat(40));
        assert!(rows[at + 1].starts_with("after"));
    }

    #[test]
    fn live_wide_characters_are_cut_on_a_column_boundary() {
        let mut harness = live_harness(21, 8);
        harness.send(out(&"日".repeat(30)));
        let rows = rows(&harness);
        let line = rows.iter().find(|r| r.starts_with('日')).expect("line");
        assert_eq!(line.trim_end(), "日".repeat(10));
    }

    #[test]
    fn live_hostile_output_leaves_the_header_and_footer_alone() {
        let mut harness = live_harness(80, 24);
        for chunk in [
            "\x1b[2J\x1b[H\x1b[31mCLEARED\x1b[0m",
            "\x1b]0;pwned\x07title",
            "\r| spin\r/ spin\rdone",
            "\x1b[999;999Hcorner",
            "a\x08\x08\x08b\x07",
        ] {
            harness.send(out(chunk));
        }
        let rows = rows(&harness);
        assert_eq!(rows[0].trim_end(), "2 Live run · following");
        assert!(rows[23].starts_with("Press ? for the key map"));
        let screen = harness.text();
        assert!(screen.contains("CLEARED"));
        assert!(screen.contains("corner"));
        assert!(!screen.contains(['\x1b', '\r', '\x07', '\x08']));
    }

    #[test]
    fn live_a_short_pane_keeps_the_status_line_and_the_newest_output() {
        let mut harness = live_harness(60, 4);
        harness.send(started(9));
        for n in 0..5 {
            harness.send(out(&format!("line {n}")));
        }
        let rows = rows(&harness);
        assert!(rows[1].starts_with("task 3 · attempt 2"));
        assert_eq!(rows[3].trim_end(), "line 4");
        assert_eq!(rows[2].trim_end(), "line 3");
    }

    #[test]
    fn live_every_small_size_draws_without_panicking_and_inside_its_area() {
        for w in 0..=30 {
            for h in 0..=8 {
                let mut harness = Harness::new(w, h);
                harness.key('2');
                harness.send(started(1));
                harness.send(gate_finished(GateKind::Lint, false, Some(1)));
                harness.send(out("日本語 \x1b[31mlong long long long long line\x1b[0m"));
                let rows = rows(&harness);
                assert_eq!(rows.len(), usize::from(h));
                for row in rows {
                    assert!(display_width(&row) <= usize::from(w), "{w}x{h}: {row:?}");
                }
            }
        }
    }

    // --- follow mode -----------------------------------------------------------

    fn press(harness: &mut Harness, code: KeyCode) {
        harness.send(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    /// A live screen of 80x24 (pane rows 4 to 22, so 19 lines) that has been
    /// fed `count` lines named `line 0`, `line 1`, and so on.
    fn filled(count: usize) -> Harness {
        let mut harness = live_harness(80, 24);
        for n in 0..count {
            harness.send(out(&format!("line {n}")));
        }
        harness
    }

    fn newest_row(harness: &Harness) -> String {
        rows(harness)[22].trim_end().to_owned()
    }

    fn header_row(harness: &Harness) -> String {
        rows(harness)[0].trim_end().to_owned()
    }

    #[test]
    fn scrolling_detaches_follow_mode() {
        for code in [KeyCode::Char('k'), KeyCode::Up, KeyCode::Char('g')] {
            let mut harness = filled(100);
            assert!(harness.app().follow);
            assert_eq!(newest_row(&harness), "line 99");
            press(&mut harness, code);
            assert!(!harness.app().follow, "{code:?} did not detach");
            assert_ne!(newest_row(&harness), "line 99", "{code:?} did not scroll");
        }
    }

    #[test]
    fn live_scrolling_up_moves_the_view_one_line_at_a_time() {
        let mut harness = filled(100);
        press(&mut harness, KeyCode::Char('k'));
        assert_eq!(newest_row(&harness), "line 98");
        assert_eq!(rows(&harness)[4].trim_end(), "line 80");
        press(&mut harness, KeyCode::Up);
        assert_eq!(newest_row(&harness), "line 97");
    }

    #[test]
    fn live_scrolling_down_returns_toward_the_newest_without_re_attaching() {
        let mut harness = filled(100);
        for _ in 0..3 {
            press(&mut harness, KeyCode::Char('k'));
        }
        assert_eq!(newest_row(&harness), "line 96");
        press(&mut harness, KeyCode::Char('j'));
        assert_eq!(newest_row(&harness), "line 97");
        press(&mut harness, KeyCode::Down);
        assert_eq!(newest_row(&harness), "line 98");
        press(&mut harness, KeyCode::Down);
        assert_eq!(newest_row(&harness), "line 99");
        press(&mut harness, KeyCode::Down);
        assert_eq!(newest_row(&harness), "line 99");
        assert!(!harness.app().follow);
    }

    #[test]
    fn live_scrolling_down_while_following_does_not_detach() {
        let mut harness = filled(100);
        for code in [KeyCode::Char('j'), KeyCode::Down, KeyCode::Char('G')] {
            press(&mut harness, code);
            assert!(harness.app().follow, "{code:?} detached");
        }
        assert_eq!(newest_row(&harness), "line 99");
    }

    #[test]
    fn live_scrolling_stops_at_the_oldest_line_and_first_and_last_jump_to_the_ends() {
        let mut harness = filled(30);
        // 30 lines in a pane of 19: 11 lines can be scrolled past.
        press(&mut harness, KeyCode::Char('g'));
        assert_eq!(rows(&harness)[4].trim_end(), "line 0");
        assert_eq!(newest_row(&harness), "line 18");
        press(&mut harness, KeyCode::Char('k'));
        assert_eq!(rows(&harness)[4].trim_end(), "line 0");
        // One line down from the top: the view has moved, so the earlier
        // presses did not pile up beyond the top.
        press(&mut harness, KeyCode::Char('j'));
        assert_eq!(rows(&harness)[4].trim_end(), "line 1");
        press(&mut harness, KeyCode::Char('G'));
        assert_eq!(newest_row(&harness), "line 29");
        assert!(!harness.app().follow);
    }

    #[test]
    fn live_scrolling_when_everything_fits_still_detaches() {
        let mut harness = filled(3);
        press(&mut harness, KeyCode::Char('k'));
        assert!(!harness.app().follow);
        assert_eq!(rows(&harness)[4].trim_end(), "line 0");
        assert_eq!(rows(&harness)[6].trim_end(), "line 2");
    }

    #[test]
    fn live_new_output_moves_the_view_while_attached() {
        let mut harness = filled(100);
        harness.send(out("fresh"));
        assert_eq!(newest_row(&harness), "fresh");
        assert_eq!(rows(&harness)[4].trim_end(), "line 82");
    }

    #[test]
    fn live_new_output_does_not_move_the_view_while_detached() {
        let mut harness = filled(100);
        press(&mut harness, KeyCode::Char('k'));
        press(&mut harness, KeyCode::Char('k'));
        let before = rows(&harness);
        assert_eq!(before[22].trim_end(), "line 97");
        for n in 0..5 {
            harness.send(out(&format!("fresh {n}")));
        }
        let after = rows(&harness);
        assert_eq!(&after[4..23], &before[4..23]);
        assert!(!harness.text().contains("fresh"));
    }

    #[test]
    fn live_a_detached_view_stays_on_its_lines_when_the_window_drops_old_ones() {
        let mut harness = live_harness(80, 24);
        for n in 0..OUTPUT_WINDOW {
            harness.send(out(&format!("line {n}")));
        }
        for _ in 0..10 {
            press(&mut harness, KeyCode::Char('k'));
        }
        assert_eq!(newest_row(&harness), "line 4085");
        for n in 0..3 {
            harness.send(out(&format!("fresh {n}")));
        }
        assert_eq!(newest_row(&harness), "line 4085");
        assert_eq!(rows(&harness)[4].trim_end(), "line 4067");
    }

    #[test]
    fn live_a_detached_view_is_not_pushed_out_by_the_ring_forgetting_its_lines() {
        let (mut app, mut journal, _scratch) = windowed(100, 100);
        app = key(app, KeyCode::Char('g'));
        assert_eq!(
            rows(&Harness::from_app(app.clone()))[4].trim_end(),
            "line 0"
        );
        for n in 0..50 {
            app = feed(app, &mut journal, &format!("fresh {n}"));
        }
        // The ring has forgotten what the view is on, so the rows say the
        // lines are not loaded rather than showing others in their place ...
        let harness = Harness::from_app(app.clone());
        assert_eq!(rows(&harness)[4].trim_end(), NOT_LOADED);
        assert!(!harness.text().contains("line 50"));
        // ... until the journal gives them back.
        assert!(backfill(&mut app, &journal).expect("backfill"));
        let harness = Harness::from_app(app.clone());
        assert_eq!(rows(&harness)[4].trim_end(), "line 0");
        let app = key(app, KeyCode::Char('j'));
        assert_eq!(rows(&Harness::from_app(app))[4].trim_end(), "line 1");
    }

    #[test]
    fn live_an_event_with_several_lines_moves_a_detached_view_by_all_of_them() {
        let mut harness = filled(100);
        press(&mut harness, KeyCode::Char('k'));
        harness.send(out("one\ntwo\nthree"));
        assert_eq!(newest_row(&harness), "line 98");
        harness.key('f');
        assert_eq!(newest_row(&harness), "three");
        assert_eq!(rows(&harness)[20].trim_end(), "one");
    }

    #[test]
    fn live_a_new_attempt_does_not_move_a_detached_view() {
        let mut harness = filled(100);
        press(&mut harness, KeyCode::Char('k'));
        harness.send(started(7));
        assert_eq!(newest_row(&harness), "line 98");
        assert!(!harness.text().contains("== task"));
    }

    #[test]
    fn live_f_re_attaches_and_shows_the_newest_output_again() {
        let mut harness = filled(100);
        for _ in 0..4 {
            press(&mut harness, KeyCode::Char('k'));
        }
        harness.send(out("fresh"));
        assert!(!harness.text().contains("fresh"));
        harness.key('f');
        assert!(harness.app().follow);
        assert_eq!(newest_row(&harness), "fresh");
        harness.send(out("fresher"));
        assert_eq!(newest_row(&harness), "fresher");
        assert_eq!(harness.app().scroll.get(&Screen::LiveRun), None);
    }

    #[test]
    fn live_f_while_following_changes_nothing() {
        let mut harness = filled(100);
        let before = harness.app().clone();
        harness.key('f');
        assert_eq!(harness.app(), &before);
    }

    #[test]
    fn live_the_title_bar_says_whether_the_pane_follows() {
        let mut harness = filled(100);
        assert_eq!(header_row(&harness), "2 Live run · following");
        press(&mut harness, KeyCode::Char('k'));
        assert_eq!(header_row(&harness), "2 Live run · detached (f to follow)");
        harness.key('f');
        assert_eq!(header_row(&harness), "2 Live run · following");
    }

    #[test]
    fn live_only_the_live_screen_shows_the_follow_state() {
        let mut harness = live_harness(80, 24);
        harness.key('1');
        assert_eq!(header_row(&harness), "1 Queue");
    }

    #[test]
    fn live_scroll_keys_leave_other_screens_and_an_open_overlay_alone() {
        let mut harness = filled(100);
        harness.key('1');
        let queue = harness.app().clone();
        harness.key('g');
        harness.key('f');
        assert!(harness.app().follow);
        assert_eq!(harness.app().scroll, queue.scroll);

        harness.key('2');
        harness.key('?');
        assert!(harness.app().overlay.is_some());
        harness.key('k');
        harness.key('g');
        assert!(harness.app().follow);
        assert_eq!(harness.app().scroll.get(&Screen::LiveRun), None);
    }

    #[test]
    fn live_the_scroll_position_is_kept_when_leaving_and_returning() {
        let mut harness = filled(100);
        for _ in 0..3 {
            press(&mut harness, KeyCode::Char('k'));
        }
        harness.key('1');
        harness.send(out("fresh"));
        harness.key('2');
        assert!(!harness.app().follow);
        assert_eq!(newest_row(&harness), "line 96");
    }

    #[test]
    fn live_a_detached_view_is_kept_within_the_pane_when_the_terminal_grows() {
        let mut harness = filled(100);
        press(&mut harness, KeyCode::Char('g'));
        harness.send(AppEvent::Resize(80, 40));
        let rows = rows(&harness);
        assert_eq!(rows[4].trim_end(), "line 0");
        assert_eq!(rows[38].trim_end(), "line 34");
    }

    #[test]
    fn live_only_the_live_screen_draws_the_output() {
        let mut harness = Harness::new(80, 24);
        harness.send(out("visible only on screen two"));
        assert!(!harness.text().contains("visible only"));
        harness.key('2');
        assert!(harness.text().contains("visible only on screen two"));
        harness.key('1');
        assert!(!harness.text().contains("visible only"));
    }

    #[test]
    fn live_the_key_map_overlay_still_draws_over_the_screen() {
        let mut harness = live_harness(80, 24);
        harness.send(out("behind"));
        harness.key('?');
        assert!(harness.text().contains("Key map"));
    }

    // --- output windowing: the ring stays at its cap, the journal has the rest

    /// A journal in its own directory under the system temp directory,
    /// removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ktask-tui-live-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn journal(&self) -> Journal {
            Journal::open(&self.0.join("journal.db")).expect("open journal")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn output_kind(text: &str) -> EventKind {
        EventKind::AgentOutput {
            attempt: AttemptId::new(1),
            stream: Stream::Stdout,
            text: text.into(),
        }
    }

    /// Records `text` as agent output in `journal` and folds the event, as
    /// read back from the journal, into `app`.
    fn feed(app: App, journal: &mut Journal, text: &str) -> App {
        let seq = journal
            .append(Some(TaskId::new(3)), &output_kind(text))
            .expect("append");
        let event = journal
            .events_since(EventSeq::new(seq.get() - 1))
            .expect("read back")
            .remove(0);
        crate::update(app, AppEvent::Core(event))
    }

    fn key(app: App, code: KeyCode) -> App {
        crate::update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    /// An 80x24 live screen (19 rows of output) with a ring of `cap` lines
    /// and a journal holding `count` lines named `line 0`, `line 1`, ...,
    /// all folded in.
    fn windowed(cap: usize, count: usize) -> (App, Journal, Scratch) {
        let scratch = Scratch::new();
        let mut journal = scratch.journal();
        let mut app = App::new((80, 24));
        app.screen = Screen::LiveRun;
        set_cap(&mut app, cap);
        for n in 0..count {
            app = feed(app, &mut journal, &format!("line {n}"));
        }
        (app, journal, scratch)
    }

    fn pane(app: &App) -> Vec<String> {
        let harness = Harness::from_app(app.clone());
        rows(&harness)[4..23]
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    fn numbered(range: Range<usize>) -> Vec<String> {
        range.map(|n| format!("line {n}")).collect()
    }

    #[test]
    fn output_ring_stays_at_its_cap() {
        let mut app = App::new((80, 24));
        app.screen = Screen::LiveRun;
        assert_eq!(app.live.cap(), OUTPUT_WINDOW);
        set_cap(&mut app, 500);
        let mut fullest = 0;
        for n in 0..100_000 {
            app = crate::update(app, out(&format!("line {n}")));
            fullest = fullest.max(app.output.len());
        }
        assert_eq!(fullest, 500);
        assert_eq!(app.output.len(), 500);
        assert_eq!(app.output.front().map(String::as_str), Some("line 99500"));
        assert_eq!(app.output.back().map(String::as_str), Some("line 99999"));
        assert_eq!(app.live.evicted(), 99_500);
    }

    #[test]
    fn output_ring_stays_at_the_default_cap_over_a_hundred_thousand_lines() {
        let mut app = App::new((80, 24));
        for n in 0..100_000 {
            app = crate::update(app, out(&format!("line {n}")));
        }
        assert_eq!(app.output.len(), OUTPUT_WINDOW);
        assert_eq!(app.live.evicted(), 100_000 - OUTPUT_WINDOW);
    }

    #[test]
    fn output_ring_is_trimmed_when_the_cap_is_lowered_and_never_below_one() {
        let (mut app, _journal, _scratch) = windowed(100, 100);
        set_cap(&mut app, 10);
        assert_eq!(app.output.len(), 10);
        assert_eq!(app.output.front().map(String::as_str), Some("line 90"));
        assert_eq!(app.live.evicted(), 90);
        set_cap(&mut app, 0);
        assert_eq!(app.live.cap(), 1);
        assert_eq!(app.output.len(), 1);
    }

    #[test]
    fn output_scrolling_back_past_the_ring_reads_older_lines_from_the_journal() {
        let (mut app, journal, _scratch) = windowed(100, 1_000);
        assert_eq!(app.output.len(), 100);
        assert_eq!(pane(&app), numbered(981..1000));
        // The oldest line of the run is far outside the ring.
        app = key(app, KeyCode::Char('g'));
        assert!(!app.follow);
        assert_eq!(pane(&app)[0], NOT_LOADED);
        assert!(backfill(&mut app, &journal).expect("backfill"));
        assert_eq!(pane(&app), numbered(0..19));
        // A page is a bounded slice, and the ring is still at its cap.
        assert!(app.live.older.lines.len() <= 100);
        assert_eq!(app.output.len(), 100);
        // The view now holds; the page covers it.
        assert_eq!(wanted(&app), None);
    }

    #[test]
    fn output_scrolling_to_the_middle_of_the_run_shows_exactly_those_lines() {
        let (mut app, journal, _scratch) = windowed(100, 1_000);
        // 500 lines above the newest: the pane ends on line 499.
        app.scroll.insert(Screen::LiveRun, 500);
        app.follow = false;
        let range = wanted(&app).expect("outside the ring");
        assert!(range.len() <= 100);
        assert!(range.start <= 481 && range.end >= 500);
        assert!(backfill(&mut app, &journal).expect("backfill"));
        assert_eq!(pane(&app), numbered(481..500));
    }

    #[test]
    fn output_wanted_is_nothing_while_the_view_is_inside_the_ring() {
        let (mut app, _journal, _scratch) = windowed(100, 1_000);
        assert_eq!(wanted(&app), None);
        app = key(app, KeyCode::Char('k'));
        assert_eq!(wanted(&app), None);
        let (fresh, _journal, _scratch) = windowed(100, 50);
        assert_eq!(wanted(&fresh), None);
    }

    #[test]
    fn output_backfill_fetches_nothing_when_nothing_is_wanted() {
        let (mut app, journal, _scratch) = windowed(100, 1_000);
        assert!(!backfill(&mut app, &journal).expect("backfill"));
        assert!(app.live.older.lines.is_empty());
    }

    #[test]
    fn output_a_detached_view_on_fetched_lines_stays_on_them_as_output_arrives() {
        let (mut app, mut journal, _scratch) = windowed(100, 1_000);
        app = key(app, KeyCode::Char('g'));
        backfill(&mut app, &journal).expect("backfill");
        for n in 0..200 {
            app = feed(app, &mut journal, &format!("fresh {n}"));
        }
        assert_eq!(pane(&app), numbered(0..19));
        // Following again shows the newest.
        app = key(app, KeyCode::Char('f'));
        assert_eq!(pane(&app)[18], "fresh 199");
    }

    #[test]
    fn output_scrolling_back_and_forth_through_pages_never_shows_a_wrong_line() {
        let (mut app, journal, _scratch) = windowed(100, 1_000);
        for up in [981, 400, 250, 120, 899, 0, 500, 300] {
            app.scroll.insert(Screen::LiveRun, up);
            app.follow = up == 0;
            backfill(&mut app, &journal).expect("backfill");
            let end = 1_000 - up;
            assert_eq!(pane(&app), numbered(end - 19..end), "up {up}");
        }
    }

    #[test]
    fn output_read_lines_numbers_lines_as_the_interface_does() {
        // Events of several lines, an empty one, and an attempt marker.
        let scratch = Scratch::new();
        let mut journal = scratch.journal();
        let mut app = App::new((80, 24));
        for text in ["a\nb\nc", "", "d"] {
            app = feed(app, &mut journal, text);
        }
        let seq = journal
            .append(
                Some(TaskId::new(3)),
                &EventKind::AttemptStarted {
                    attempt: AttemptId::new(2),
                    protocol: "tdd".into(),
                    pid: 9,
                    base_sha: "abc".into(),
                },
            )
            .expect("append");
        for event in journal
            .events_since(EventSeq::new(seq.get() - 1))
            .expect("read")
        {
            app = crate::update(app, AppEvent::Core(event));
        }
        app = feed(app, &mut journal, "e\nf");
        let seen: Vec<String> = app.output.iter().cloned().collect();
        assert_eq!(seen.len(), 8);
        assert_eq!(read_lines(&journal, 0..8).expect("read"), seen);
        assert_eq!(read_lines(&journal, 2..5).expect("read"), seen[2..5]);
        assert_eq!(read_lines(&journal, 5..100).expect("read"), seen[5..]);
        assert!(read_lines(&journal, 8..10).expect("read").is_empty());
    }

    #[test]
    fn output_lines_fetched_from_the_journal_are_as_safe_to_draw_as_live_ones() {
        let (mut app, journal, _scratch) = windowed(10, 0);
        let mut journal = journal;
        app = feed(app, &mut journal, "\x1b[2J\x1b]0;pwned\x07boom");
        for n in 0..30 {
            app = feed(app, &mut journal, &format!("line {n}"));
        }
        app = key(app, KeyCode::Char('g'));
        backfill(&mut app, &journal).expect("backfill");
        let shown = pane(&app).join("\n");
        assert!(shown.contains("boom"));
        assert!(shown.chars().all(|c| !c.is_control() || c == '\n'));
    }
}

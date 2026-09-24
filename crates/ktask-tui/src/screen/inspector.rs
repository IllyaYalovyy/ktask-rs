//! The task inspector: one task in full, with per-attempt evidence.
//!
//! The screen shows the task the queue selected (or, once the operator moves
//! with `j`/`k`, the one they moved to) and reads top to bottom:
//!
//! - what the task *is*: its outcome, done-when, verify and refs, as authored
//!   (see [`Inspector::set_definition`]);
//! - the work protocol, one chip per phase, with the phase the journal says
//!   the task is in highlighted, and beneath it what that phase may write and
//!   which gate it runs (VISION.md §9);
//! - the completion gates and where the shown attempt stands with each;
//! - the evidence of one attempt: verdict, gate results, timing, exit, model,
//!   commits and usage. `[` and `]` step to the previous and the next attempt;
//!   until the operator steps back, the newest attempt is shown, and it stays
//!   the one shown as newer ones arrive.
//!
//! [`Inspector`] folds the journal itself, per task and per attempt. Nothing
//! here is taken from the [`TaskView`] but the title,
//! the state and the protocol's name, so the highlighted phase is exactly the
//! last `PhaseEntered` the journal holds for the attempt shown. A phase is
//! *done* once the attempt has entered it and moved on, *current* while it is
//! the newest thing the running task has entered, *stopped* when the attempt
//! ended in it without succeeding, *skipped* when a declared TDD exception
//! spared the task its red phase, and *pending* otherwise. A phase the journal
//! shows that the protocol does not list is still drawn, after the listed
//! ones, so the screen can never disagree with the journal.
//!
//! The journal's `AttemptRecorded` event is the attempt's durable evidence and
//! overrides what the earlier events of the attempt said; until it arrives the
//! screen shows what those events did. Every text that reaches the screen from
//! the journal or the task file is sanitized when it is stored and cut at the
//! edge of the pane when it is drawn.
//!
//! The height the screen has is shared out by priority. The title, the state
//! line, the protocol and the attempt's heading come first, then one line of
//! each part of the definition, then the rest is dealt out one line at a time
//! between the definition and the evidence, so a short terminal loses the tail
//! of each rather than the whole of one.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::LayoutPlan;
use crate::sanitize::sanitize;
use crate::screen::failures::class_name;
use crate::screen::live::{GateLine, gate_name};
use crate::screen::logs::phase_name;
use crate::text::{display_width, truncate_to_width};
use crate::types::{Screen, TaskView};
use crossterm::event::KeyEvent;
use ktask_core::{
    AttemptId, Event, EventKind, FailureClass, GateKind, GateResult, Phase, PhaseSpec, Protocol,
    Task, TaskId, Usage, WriteScope,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};
use unicode_segmentation::UnicodeSegmentation;

/// The longest text kept from a definition field, a detail or a gate's output.
const MAX_TEXT_CHARS: usize = 2_048;

/// What the body shows when there is no task to inspect.
const EMPTY: &str = "No task to inspect.";

/// What the outcome field says when the task's definition was never given.
const NO_DEFINITION: &str = "(the task's definition is not loaded)";

/// What the evidence says when a task has had no attempt.
const NO_ATTEMPTS: &str = "No attempts yet.";

/// The key hint on the last row.
const HINT: &str = "[ / ] attempt  j / k task  g / G first / last";

/// What separates the entries of a line.
const SEPARATOR: &str = " · ";

/// The columns the label of a definition field takes, with its gap.
const LABEL_WIDTH: usize = 11;

/// The labels of the definition's fields and the most lines each may take.
const FIELDS: [(&str, usize); 4] = [("Outcome", 3), ("Done-when", 4), ("Verify", 2), ("Refs", 2)];

/// The most evidence lines wanted for one attempt.
const EVIDENCE_LINES: usize = 12;

/// The fixed lines around the definition and the evidence: the title, the
/// state, the protocol's phases, the attempt's heading, the completion gates,
/// the current phase's detail and the key hint.
const FIXED_LINES: u16 = 7;

/// The rows kept for the first line of each of the definition's fields, out of
/// the ones the fixed lines would take.
const RESERVED_ROWS: u16 = 4;

/// The gates that complete a task, in the order the core runs them.
const COMPLETION: [GateKind; 5] = [
    GateKind::Format,
    GateKind::Lint,
    GateKind::Build,
    GateKind::Verify,
    GateKind::Privacy,
];

/// The task's definition as its file gives it, sanitized.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Definition {
    outcome: String,
    done_when: String,
    verify: String,
    refs: String,
}

/// What became of a task, once the journal says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

/// How verification judged an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Passed,
    Failed { class: FailureClass, detail: String },
}

/// A gate's result as an attempt's evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateEntry {
    line: GateLine,
    /// The last line the gate wrote, for the reason it failed.
    tail: String,
}

impl GateEntry {
    fn new(result: &GateResult) -> Self {
        let last = |text: &str| {
            sanitize(text)
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_owned())
        };
        let tail = last(&result.stderr)
            .or_else(|| last(&result.stdout))
            .unwrap_or_default();
        Self {
            line: GateLine::new(result),
            tail: clip(&tail),
        }
    }
}

/// One attempt at a task, as the journal tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attempt {
    id: AttemptId,
    /// Whether the attempt is a remediation round rather than the first.
    remediation: bool,
    protocol: Option<String>,
    started: Option<OffsetDateTime>,
    ended: Option<OffsetDateTime>,
    /// The phases entered, in the order they were first entered.
    phases: Vec<Phase>,
    /// The phase entered last.
    current: Option<Phase>,
    gates: Vec<GateEntry>,
    exit_code: Option<i32>,
    exit_reason: Option<String>,
    model_configured: Option<String>,
    model_reported: Option<String>,
    session_id: Option<String>,
    usage: Option<String>,
    base_sha: Option<String>,
    candidate_sha: Option<String>,
    verdict: Option<Verdict>,
}

impl Attempt {
    fn new(id: AttemptId) -> Self {
        Self {
            id,
            remediation: false,
            protocol: None,
            started: None,
            ended: None,
            phases: Vec::new(),
            current: None,
            gates: Vec::new(),
            exit_code: None,
            exit_reason: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            usage: None,
            base_sha: None,
            candidate_sha: None,
            verdict: None,
        }
    }
}

/// Everything the journal says about one task.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct History {
    /// The attempts by number, so the newest is the last.
    attempts: BTreeMap<AttemptId, Attempt>,
    end: Option<End>,
    /// The declared TDD exception, if one spared the task its red phase.
    exception: Option<String>,
    /// The gate that has started and not yet finished.
    running_gate: Option<GateKind>,
    /// The attempt shown; the newest when `None`.
    cursor: Option<usize>,
}

impl History {
    /// The attempt numbered `id`, added if the journal has not shown it yet.
    fn attempt(&mut self, id: AttemptId) -> &mut Attempt {
        self.attempts.entry(id).or_insert_with(|| Attempt::new(id))
    }

    /// The `at`th attempt, from zero, oldest first.
    fn nth(&self, at: usize) -> Option<&Attempt> {
        self.attempts.values().nth(at)
    }

    /// The newest attempt.
    fn newest(&self) -> Option<&Attempt> {
        self.attempts.values().next_back()
    }

    /// The index of the attempt shown, if there is any.
    fn viewing(&self) -> Option<usize> {
        let last = self.attempts.len().checked_sub(1)?;
        Some(self.cursor.map_or(last, |at| at.min(last)))
    }

    /// Moves the attempt shown by `by` places, stopping at either end; the
    /// newest attempt is followed again once it is reached.
    fn step(&mut self, by: isize) {
        let Some(last) = self.attempts.len().checked_sub(1) else {
            return;
        };
        let at = self
            .viewing()
            .unwrap_or(0)
            .saturating_add_signed(by)
            .min(last);
        self.cursor = (at < last).then_some(at);
    }

    /// Folds one event of this task in.
    fn apply(&mut self, at: OffsetDateTime, kind: &EventKind) {
        match kind {
            EventKind::AttemptStarted {
                attempt,
                protocol,
                base_sha,
                ..
            } => {
                self.begin();
                let entry = self.attempt(*attempt);
                entry.started = Some(at);
                entry.protocol = Some(clip(protocol));
                entry.base_sha = Some(clip(base_sha));
            }
            EventKind::RetryStarted { attempt } => {
                self.begin();
                let entry = self.attempt(*attempt);
                entry.remediation = true;
                entry.started.get_or_insert(at);
            }
            EventKind::Resumed => self.begin(),
            EventKind::PhaseEntered { attempt, phase } => {
                let entry = self.attempt(*attempt);
                if !entry.phases.contains(phase) {
                    entry.phases.push(*phase);
                }
                entry.current = Some(*phase);
            }
            EventKind::GateStarted { gate } => self.running_gate = Some(*gate),
            EventKind::GateFinished { result } => {
                self.running_gate = None;
                if let Some(entry) = self.attempts.values_mut().next_back() {
                    entry.gates.push(GateEntry::new(result));
                }
            }
            EventKind::TddExceptionUsed { exception, reason } => {
                self.exception = Some(clip(&format!("{exception:?}: {reason}")));
            }
            EventKind::TaskDone { .. } => self.finish(End::Done),
            EventKind::TaskFailed { .. } => self.finish(End::Failed),
            EventKind::TaskCancelled { .. } => self.finish(End::Cancelled),
            EventKind::Interrupted { .. } => self.finish(End::Interrupted),
            other => self.apply_evidence(other),
        }
    }

    /// Folds the events that carry an attempt's evidence in.
    fn apply_evidence(&mut self, kind: &EventKind) {
        match kind {
            EventKind::AttemptFinished {
                attempt,
                exit_code,
                usage,
                session_id,
                model_reported,
            } => {
                let entry = self.attempt(*attempt);
                entry.exit_code = Some(*exit_code);
                entry.usage = usage.as_ref().map(describe_usage);
                entry.session_id = session_id.as_deref().map(clip);
                entry.model_reported = model_reported.as_deref().map(clip);
            }
            EventKind::VerifyPassed { attempt } => {
                self.attempt(*attempt).verdict = Some(Verdict::Passed);
            }
            EventKind::VerifyFailed {
                attempt,
                class,
                detail,
            } => {
                self.attempt(*attempt).verdict = Some(Verdict::Failed {
                    class: *class,
                    detail: clip(detail),
                });
            }
            EventKind::PublishStarted {
                attempt,
                candidate_sha,
            } => self.attempt(*attempt).candidate_sha = Some(clip(candidate_sha)),
            EventKind::AttemptRecorded { record } => {
                let entry = self.attempt(record.id);
                entry.started = Some(record.started);
                entry.ended = record.ended;
                entry.exit_reason = Some(clip(&record.exit_reason));
                entry.model_configured = record.model_configured.as_deref().map(clip);
                entry.model_reported = record.model_reported.as_deref().map(clip);
                entry.session_id = record.session_id.as_deref().map(clip);
                entry.usage = record.usage.as_ref().map(describe_usage);
                entry.base_sha = Some(clip(&record.base_sha));
                entry.candidate_sha = record.candidate_sha.as_deref().map(clip);
                entry.gates = record.gates.iter().map(GateEntry::new).collect();
            }
            _ => {}
        }
    }

    /// Notes that work on the task is under way again.
    fn begin(&mut self) {
        self.end = None;
        self.running_gate = None;
    }

    /// Notes how the task ended.
    fn finish(&mut self, end: End) {
        self.end = Some(end);
        self.running_gate = None;
    }
}

/// The token and cost figures of a usage record, in words.
fn describe_usage(usage: &Usage) -> String {
    let mut parts = Vec::new();
    for (count, what) in [
        (usage.input_tokens, "in"),
        (usage.output_tokens, "out"),
        (usage.cached_tokens, "cached"),
    ] {
        parts.extend(count.map(|count| format!("{count} {what}")));
    }
    parts.extend(usage.cost_usd.map(|cost| format!("${cost:.2}")));
    if parts.is_empty() {
        "not reported".to_owned()
    } else {
        parts.join(SEPARATOR)
    }
}

/// `text` sanitized, on one line and no longer than [`MAX_TEXT_CHARS`].
fn clip(text: &str) -> String {
    let clean = sanitize(text);
    let joined = clean.trim_end_matches('\n').replace('\n', " ⏎ ");
    match joined.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", joined.get(..end).unwrap_or_default()),
        None => joined,
    }
}

/// `text` sanitized and no longer than [`MAX_TEXT_CHARS`], keeping its lines.
fn clip_lines(text: &str) -> String {
    let clean = sanitize(text);
    match clean.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", clean.get(..end).unwrap_or_default()),
        None => clean,
    }
}

/// What the inspector knows: each task's definition and the journal's account
/// of its attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspector {
    definitions: BTreeMap<TaskId, Definition>,
    histories: BTreeMap<TaskId, History>,
    completion: Vec<GateKind>,
}

impl Default for Inspector {
    fn default() -> Self {
        Self {
            definitions: BTreeMap::new(),
            histories: BTreeMap::new(),
            completion: COMPLETION.to_vec(),
        }
    }
}

impl Inspector {
    /// Records the definition of `task`, replacing any earlier one. The
    /// journal does not carry it, so whatever loads the queue hands it here.
    pub fn set_definition(&mut self, task: &Task) {
        self.definitions.insert(
            task.id,
            Definition {
                outcome: clip_lines(&task.outcome),
                done_when: clip_lines(&task.done_when),
                verify: clip_lines(&task.verify),
                refs: clip_lines(&task.refs),
            },
        );
    }

    /// Sets the gates the completion row lists, in the order they run. It
    /// starts as the core's own: format, lint, build, verify and privacy.
    pub fn set_completion_gates(&mut self, gates: Vec<GateKind>) {
        self.completion = gates;
    }

    /// How many attempts the journal has shown for `task`.
    #[must_use]
    pub fn attempt_count(&self, task: TaskId) -> usize {
        self.histories
            .get(&task)
            .map_or(0, |history| history.attempts.len())
    }

    /// The position, from zero, of the attempt of `task` the screen shows, or
    /// `None` when it has had none.
    #[must_use]
    pub fn viewing(&self, task: TaskId) -> Option<usize> {
        self.histories.get(&task)?.viewing()
    }

    /// The phase `task` is in now: the last one the journal shows its newest
    /// attempt entering, unless the task has since ended.
    #[must_use]
    pub fn current_phase(&self, task: TaskId) -> Option<Phase> {
        let history = self.histories.get(&task)?;
        match history.end {
            None => history.newest()?.current,
            Some(_) => None,
        }
    }
}

/// Folds one journal event into the inspector.
pub fn fold(app: &mut App, event: &Event) {
    if let Some(task) = event.task_id {
        app.inspector
            .histories
            .entry(task)
            .or_default()
            .apply(event.ts, &event.kind);
    }
}

/// The row of the queue the inspector shows: the stored index, or the last
/// row when the queue has become shorter than it. `None` for an empty queue.
fn selected_row(app: &App) -> Option<usize> {
    let last = app.tasks.len().checked_sub(1)?;
    Some(
        app.selected
            .get(&Screen::Inspector)
            .map_or(0, |at| (*at).min(last)),
    )
}

/// Whether the inspector has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::Inspector && app.overlay.is_none()
}

/// Handles the inspector's keys: `[` and `]` step through the attempts of the
/// selected task, and `j`, `k`, the arrows, `g` and `G` move to another task.
/// Does nothing on other screens, under an overlay, or for other keys. Any key
/// press here first clears the notice the last one left.
///
/// Both stop at their ends rather than wrapping.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    app.notice = None;
    let (Some(current), Some(last)) = (selected_row(app), app.tasks.len().checked_sub(1)) else {
        return;
    };
    let action = lookup(app.screen, key).map(|binding| binding.action);
    let next = match action {
        Some(KeyAction::MoveDown) => (current + 1).min(last),
        Some(KeyAction::MoveUp) => current.saturating_sub(1),
        Some(KeyAction::First) => 0,
        Some(KeyAction::Last) => last,
        Some(KeyAction::PrevAttempt | KeyAction::NextAttempt) => {
            let by = if action == Some(KeyAction::NextAttempt) {
                1
            } else {
                -1
            };
            if let Some(task) = app.tasks.get(current) {
                app.inspector.histories.entry(task.id).or_default().step(by);
            }
            return;
        }
        _ => return,
    };
    app.selected.insert(Screen::Inspector, next);
}

/// How a phase stands in the shown attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Done,
    Current,
    Stopped,
    Pending,
    Skipped,
}

impl Mark {
    fn glyph(self) -> &'static str {
        match self {
            Mark::Done => "✓",
            Mark::Current => "▶",
            Mark::Stopped => "✗",
            Mark::Pending => "·",
            Mark::Skipped => "↷",
        }
    }

    fn style(self) -> Style {
        match self {
            Mark::Done => Style::new().fg(Color::Green),
            Mark::Current => Style::new()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            Mark::Stopped => Style::new().fg(Color::Red),
            Mark::Pending | Mark::Skipped => Style::new().add_modifier(Modifier::DIM),
        }
    }
}

impl History {
    /// Whether `attempt` is a remediation round that carries on from an
    /// earlier attempt that had already been through `phase`: a round starts
    /// where the failure was, not from the top of the protocol.
    fn carried(&self, attempt: &Attempt, phase: Phase) -> bool {
        attempt.remediation
            && self
                .attempts
                .range(..attempt.id)
                .any(|(_, earlier)| earlier.phases.contains(&phase))
    }

    /// How `phase` stands in `attempt`, which is the newest one when
    /// `newest`. The phase entered last is current while the task runs, done
    /// once the task is, and stopped when the attempt or the task failed in
    /// it; an older attempt got past every phase but the one its verification
    /// rejected. A phase the attempt never entered is pending, or skipped if
    /// a TDD exception spared the task its red phase.
    fn mark(&self, attempt: &Attempt, newest: bool, phase: Phase) -> Mark {
        if !attempt.phases.contains(&phase) {
            let spared = phase == Phase::Red && self.exception.is_some();
            return if self.carried(attempt, phase) {
                Mark::Done
            } else if spared {
                Mark::Skipped
            } else {
                Mark::Pending
            };
        }
        if attempt.current != Some(phase) {
            return Mark::Done;
        }
        if matches!(attempt.verdict, Some(Verdict::Failed { .. })) {
            return Mark::Stopped;
        }
        match (newest, self.end) {
            (true, None) => Mark::Current,
            (true, Some(End::Done)) | (false, _) => Mark::Done,
            (true, Some(_)) => Mark::Stopped,
        }
    }
}

/// The phases of the protocol named `name`, in order; none if the name is
/// not one the core knows.
fn protocol_specs(name: &str) -> Vec<PhaseSpec> {
    match name {
        "direct" => Protocol::direct().phases,
        "tdd" => Protocol::tdd().phases,
        _ => Vec::new(),
    }
}

/// What a phase may write, in words.
fn scope_name(scope: WriteScope) -> &'static str {
    match scope {
        WriteScope::All => "may write any path",
        WriteScope::TestsOnly => "may write tests only",
        WriteScope::None => "read-only",
    }
}

/// `text` cut to `width` columns, as one line.
fn fit(line: Line<'static>, width: usize) -> Line<'static> {
    let mut left = width;
    let mut spans = Vec::new();
    for span in line.spans {
        if left == 0 {
            break;
        }
        let text = truncate_to_width(&span.content, left);
        left -= display_width(&text);
        spans.push(Span::styled(text, span.style));
    }
    Line::from(spans).style(line.style)
}

/// `text` broken into rows of at most `width` columns, at spaces where it can
/// be and inside a word where it cannot. Its own line breaks are kept; blank
/// lines are dropped.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut row = String::new();
        let mut used = 0;
        for cluster in line.trim_end().graphemes(true) {
            let size = display_width(cluster);
            if used + size > width && !row.is_empty() {
                let carried = match row.rfind(' ') {
                    Some(space) if space > 0 && cluster != " " => {
                        row.split_off(space).trim_start().to_owned()
                    }
                    _ => String::new(),
                };
                if !row.trim().is_empty() {
                    rows.push(row.trim_end().to_owned());
                }
                used = display_width(&carried);
                row = carried;
                if cluster == " " {
                    continue;
                }
            }
            row.push_str(cluster);
            used += size;
        }
        rows.push(row);
    }
    rows
}

/// How many of `wants` lines each item gets out of `budget`, dealt out one
/// line at a time in order, so every item has a line before any has a second.
fn share(wants: &[usize], mut budget: usize) -> Vec<usize> {
    let mut given = vec![0; wants.len()];
    loop {
        let before = budget;
        for (want, got) in wants.iter().zip(&mut given) {
            if *got < *want && budget > 0 {
                *got += 1;
                budget -= 1;
            }
        }
        if budget == before {
            return given;
        }
    }
}

/// `time` as `2024-01-15 10:00:00`.
fn stamp(time: OffsetDateTime) -> String {
    format!(
        "{} {:02}:{:02}:{:02}",
        time.date(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

/// `span` as `45s`, `5m00s` or `1h02m03s`.
fn duration(span: Duration) -> String {
    let seconds = span.whole_seconds().max(0);
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds:02}s"),
        _ => format!("{hours}h{minutes:02}m{seconds:02}s"),
    }
}

/// The first eight characters of a commit.
fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// The style of the labels that start a line.
fn label_style() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// `line` moved right by the label column, so it lines up with the values.
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(LABEL_WIDTH))];
    spans.extend(line.spans);
    Line::from(spans).style(line.style)
}

/// A line drawn dimly.
fn dim(text: impl Into<String>) -> Line<'static> {
    Line::styled(text.into(), Style::new().add_modifier(Modifier::DIM))
}

/// `label` padded to the width of the label column.
fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<LABEL_WIDTH$}"), label_style())
}

/// The first `count` of `rows` as the lines of a definition field, the label
/// on the first, ending in an ellipsis if some were left out.
fn field_lines(name: &str, rows: &[String], count: usize, width: usize) -> Vec<Line<'static>> {
    let cut = count < rows.len();
    rows.iter()
        .take(count)
        .enumerate()
        .map(|(at, row)| {
            let row = if cut && at + 1 == count {
                let kept = width.saturating_sub(LABEL_WIDTH + 1);
                format!("{}…", truncate_to_width(row, kept))
            } else {
                row.clone()
            };
            let lead = if at == 0 {
                label(name)
            } else {
                Span::raw(" ".repeat(LABEL_WIDTH))
            };
            Line::from(vec![lead, Span::raw(row)])
        })
        .collect()
}

/// The lines of evidence for `attempt`, most telling first.
fn evidence(history: &History, attempt: &Attempt, newest: bool) -> Vec<Line<'static>> {
    let mut lines = vec![match &attempt.verdict {
        Some(Verdict::Passed) => Line::styled("verdict: passed", Style::new().fg(Color::Green)),
        Some(Verdict::Failed { class, detail }) => Line::styled(
            format!("verdict: failed{SEPARATOR}{}: {detail}", class_name(*class)),
            Style::new().fg(Color::Red),
        ),
        None => dim("verdict: none recorded"),
    }];
    for entry in &attempt.gates {
        let ink = if entry.line.passed {
            Color::Green
        } else {
            Color::Red
        };
        lines.push(Line::styled(
            format!("gate {}", entry.line.describe()),
            Style::new().fg(ink),
        ));
        if !entry.line.passed && !entry.tail.is_empty() {
            lines.push(dim(format!("  └ {}", entry.tail)));
        }
    }
    lines.push(Line::raw(timing(history, attempt, newest)));
    let exit: Vec<String> = attempt
        .exit_reason
        .iter()
        .cloned()
        .chain(
            attempt
                .exit_code
                .map(|code| format!("provider exit {code}")),
        )
        .collect();
    if !exit.is_empty() {
        lines.push(Line::raw(format!("exit: {}", exit.join(SEPARATOR))));
    }
    lines.extend(model_lines(attempt));
    if let Some(base) = &attempt.base_sha {
        let candidate = attempt
            .candidate_sha
            .as_deref()
            .map_or_else(|| "no candidate".to_owned(), short);
        lines.push(Line::raw(format!("commits: {} → {candidate}", short(base))));
    }
    lines.extend(
        attempt
            .usage
            .iter()
            .map(|usage| Line::raw(format!("usage: {usage}"))),
    );
    lines.truncate(EVIDENCE_LINES);
    lines
}

/// The line saying when the attempt started, ended and how long it took.
fn timing(history: &History, attempt: &Attempt, newest: bool) -> String {
    let mut parts = Vec::new();
    parts.extend(attempt.started.map(|at| format!("started {}", stamp(at))));
    parts.extend(attempt.ended.map(|at| format!("ended {}", stamp(at))));
    if let (Some(started), Some(ended)) = (attempt.started, attempt.ended) {
        parts.push(format!("took {}", duration(ended - started)));
    } else if newest && history.end.is_none() {
        parts.push("running".to_owned());
    }
    if parts.is_empty() {
        "timing: unknown".to_owned()
    } else {
        parts.join(SEPARATOR)
    }
}

/// The lines naming the model and the session, if the journal named them.
fn model_lines(attempt: &Attempt) -> Vec<Line<'static>> {
    let model = match (&attempt.model_reported, &attempt.model_configured) {
        (Some(reported), Some(configured)) if reported != configured => {
            Some(format!("model {reported} (configured {configured})"))
        }
        (Some(model), _) | (None, Some(model)) => Some(format!("model {model}")),
        (None, None) => None,
    };
    let session = attempt
        .session_id
        .as_ref()
        .map(|id| format!("session {id}"));
    model.into_iter().chain(session).map(Line::raw).collect()
}

/// The phases to draw for the shown attempt, each with its mark: the
/// protocol's own, in order, then any the journal shows that it does not list.
fn phase_marks(
    history: &History,
    attempt: Option<&Attempt>,
    newest: bool,
    specs: &[PhaseSpec],
) -> Vec<(Phase, Mark)> {
    let listed = specs.iter().map(|spec| spec.phase);
    let extra = attempt
        .into_iter()
        .flat_map(|attempt| attempt.phases.iter().copied())
        .filter(|phase| !specs.iter().any(|spec| spec.phase == *phase));
    listed
        .chain(extra)
        .map(|phase| {
            let mark = attempt.map_or(Mark::Pending, |attempt| {
                history.mark(attempt, newest, phase)
            });
            (phase, mark)
        })
        .collect()
}

/// The row of protocol chips: each phase with its mark, the one the task is
/// in highlighted.
fn phase_chips(marks: &[(Phase, Mark)], name: &str) -> Line<'static> {
    let mut spans = vec![label("Protocol")];
    if marks.is_empty() {
        let name = if name.is_empty() { "unknown" } else { name };
        spans.push(Span::styled(
            format!("{name}: no phases known"),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    for (at, (phase, mark)) in marks.iter().enumerate() {
        if at > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{} {}", mark.glyph(), phase_name(*phase).to_lowercase()),
            mark.style(),
        ));
    }
    Line::from(spans)
}

/// The line under the chips: what the phase the attempt is in may write and
/// which gate it runs.
fn phase_detail(specs: &[PhaseSpec], attempt: Option<&Attempt>) -> Line<'static> {
    let Some(phase) = attempt.and_then(|attempt| attempt.current) else {
        return dim(format!("{:LABEL_WIDTH$}no phase entered yet", ""));
    };
    let mut parts = vec![format!("in {}", phase_name(phase).to_lowercase())];
    if let Some(spec) = specs.iter().find(|spec| spec.phase == phase) {
        parts.push(scope_name(spec.write_scope).to_owned());
        parts.push(spec.gate.map_or_else(
            || "no gate".to_owned(),
            |gate| format!("gate {}", gate_name(gate)),
        ));
        if spec.records_evidence {
            parts.push("records evidence".to_owned());
        }
    }
    Line::raw(format!("{:LABEL_WIDTH$}{}", "", parts.join(SEPARATOR)))
}

/// The completion gates row: each gate marked by what the shown attempt's
/// last result for it was.
fn gate_row(gates: &[GateKind], history: &History, at: Option<usize>) -> Line<'static> {
    let attempt = at.and_then(|at| history.nth(at));
    let newest = at.is_some_and(|at| at + 1 == history.attempts.len());
    let mut spans = vec![label("Gates")];
    for (index, kind) in gates.iter().enumerate() {
        let last = attempt.and_then(|attempt| {
            attempt
                .gates
                .iter()
                .rev()
                .find(|entry| entry.line.kind == *kind)
        });
        let mark = match last {
            Some(entry) if entry.line.passed => Mark::Done,
            Some(_) => Mark::Stopped,
            None if newest && history.running_gate == Some(*kind) => Mark::Current,
            None => Mark::Pending,
        };
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{} {}", mark.glyph(), gate_name(*kind)),
            mark.style(),
        ));
    }
    Line::from(spans)
}

/// The heading of the evidence: which attempt is shown.
fn heading(history: &History, at: Option<usize>) -> Line<'static> {
    let shown = at.and_then(|at| Some((at, history.nth(at)?)));
    let Some((at, attempt)) = shown else {
        return Line::from(vec![label("Attempts"), Span::raw("none yet")]);
    };
    let count = history.attempts.len();
    let mut text = format!("{} of {count}{SEPARATOR}attempt {}", at + 1, attempt.id);
    if attempt.remediation {
        text.push_str(" (remediation)");
    }
    text.push_str(if at + 1 == count {
        " · newest"
    } else {
        " · older"
    });
    Line::from(vec![label("Attempts"), Span::raw(text)])
}

/// The name of the protocol the task runs under: the queue's, or else the
/// one its shown attempt started under.
fn protocol_name(view: &TaskView, attempt: Option<&Attempt>) -> String {
    if view.protocol.is_empty() {
        attempt
            .and_then(|attempt| attempt.protocol.clone())
            .unwrap_or_default()
    } else {
        view.protocol.clone()
    }
}

/// The rows of each definition field, wrapped for `width` columns.
fn definition_rows(definition: Option<&Definition>, width: usize) -> Vec<Vec<String>> {
    let fields = definition.map_or_else(
        || [NO_DEFINITION, "", "", ""].map(str::to_owned),
        |def| {
            [
                def.outcome.clone(),
                def.done_when.clone(),
                def.verify.clone(),
                def.refs.clone(),
            ]
        },
    );
    fields
        .iter()
        .map(|text| wrap(text, width.saturating_sub(LABEL_WIDTH)))
        .collect()
}

/// The lines of the screen for `view`, top to bottom, fitted to `width` and
/// at most `height` of them, and whether the last row is the key hint.
fn screen_lines(
    app: &App,
    view: &TaskView,
    width: usize,
    height: u16,
) -> (Vec<Line<'static>>, bool) {
    let empty = History::default();
    let history = app.inspector.histories.get(&view.id).unwrap_or(&empty);
    let at = history.viewing();
    let attempt = at.and_then(|at| history.nth(at));
    let newest = at.is_some_and(|at| at + 1 == history.attempts.len());
    let name = protocol_name(view, attempt);
    let specs = protocol_specs(&name);
    let marks = phase_marks(history, attempt, newest, &specs);

    let fixed = FIXED_LINES.min(height.saturating_sub(RESERVED_ROWS)).max(1);
    let rows = definition_rows(app.inspector.definitions.get(&view.id), width);
    let mut evidence_lines = match attempt {
        Some(attempt) => evidence(history, attempt, newest),
        None => vec![dim(NO_ATTEMPTS)],
    };
    if fixed < 4 {
        evidence_lines.clear();
    }
    let mut wants: Vec<usize> = rows
        .iter()
        .zip(FIELDS)
        .map(|(rows, (_, most))| rows.len().min(most))
        .collect();
    wants.push(evidence_lines.len());
    let given = share(&wants, usize::from(height - fixed));

    let mut lines = vec![Line::styled(
        format!(" Task {}{SEPARATOR}{}", view.id, clip(&view.title)),
        label_style(),
    )];
    if fixed >= 2 {
        let count = history
            .attempts
            .len()
            .max(usize::try_from(view.attempts).unwrap_or(usize::MAX));
        lines.push(Line::raw(format!(
            " {}{SEPARATOR}{}{SEPARATOR}{count} {}",
            view.state,
            if name.is_empty() { "-" } else { &name },
            if count == 1 { "attempt" } else { "attempts" }
        )));
    }
    for (((name, _), rows), count) in FIELDS.iter().zip(&rows).zip(&given) {
        lines.extend(field_lines(name, rows, *count, width));
    }
    if fixed >= 3 {
        lines.push(phase_chips(&marks, &name));
    }
    if fixed >= 6 {
        lines.push(phase_detail(&specs, attempt));
    }
    if fixed >= 5 {
        lines.push(gate_row(&app.inspector.completion, history, at));
    }
    if fixed >= 4 {
        lines.push(heading(history, at));
    }
    evidence_lines.truncate(given.last().copied().unwrap_or(0));
    lines.extend(evidence_lines.into_iter().map(indent));
    let lines = lines.into_iter().map(|line| fit(line, width)).collect();
    (lines, fixed >= FIXED_LINES)
}

/// Draws the inspector into the body of `plan`: the selected task's
/// definition, protocol, completion gates and the evidence of one attempt,
/// each where the body is tall enough for it.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let Some(view) = selected_row(app).and_then(|row| app.tasks.get(row)) else {
        frame.render_widget(
            Paragraph::new(truncate_to_width(EMPTY, width))
                .style(Style::new().add_modifier(Modifier::DIM)),
            body,
        );
        return;
    };
    let (lines, hint) = screen_lines(app, view, width, body.height);
    let hint_rows = u16::from(hint);
    let [main, hint_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(hint_rows)]).areas(body);
    frame.render_widget(Paragraph::new(lines), main);
    if hint {
        frame.render_widget(
            Paragraph::new(truncate_to_width(HINT, width)).style(Style::new().fg(Color::DarkGray)),
            hint_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::update;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::TaskView;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ktask_core::{AttemptRecord, EventSeq, TaskStatus, UsageSource};
    use time::macros::datetime;

    fn view(id: u32, state: &str) -> TaskView {
        TaskView {
            id: TaskId::new(id),
            title: format!("Task {id} title"),
            state: state.to_owned(),
            protocol: "tdd".to_owned(),
            phase: None,
            attempts: 0,
            elapsed: None,
        }
    }

    fn task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id} title"),
            outcome: "a task's full definition and history are visible".to_owned(),
            done_when: "snapshots cover a task with no attempts, one attempt, and several"
                .to_owned(),
            verify: "cargo nextest run -p ktask-tui".to_owned(),
            refs: "VISION.md sections 9 and 13".to_owned(),
            protocol: Some("tdd".to_owned()),
        }
    }

    fn app_with(tasks: Vec<TaskView>, size: (u16, u16)) -> App {
        let mut app = App {
            screen: Screen::Inspector,
            tasks,
            ..App::new(size)
        };
        for id in 1..=3 {
            app.inspector.set_definition(&task(id));
        }
        app
    }

    fn event(task: u32, kind: EventKind) -> AppEvent {
        AppEvent::Core(Event {
            seq: EventSeq::new(1),
            ts: datetime!(2024-01-15 10:00:00 UTC),
            task_id: Some(TaskId::new(task)),
            kind,
        })
    }

    fn feed(app: App, task: u32, kinds: Vec<EventKind>) -> App {
        kinds
            .into_iter()
            .fold(app, |app, kind| update(app, event(task, kind)))
    }

    fn started(attempt: u32) -> EventKind {
        EventKind::AttemptStarted {
            attempt: AttemptId::new(attempt),
            protocol: "tdd".to_owned(),
            pid: 7,
            base_sha: "abc1234567890".to_owned(),
        }
    }

    fn entered(attempt: u32, phase: Phase) -> EventKind {
        EventKind::PhaseEntered {
            attempt: AttemptId::new(attempt),
            phase,
        }
    }

    fn result(kind: GateKind, passed: bool, stderr: &str) -> GateResult {
        GateResult {
            kind,
            passed,
            exit_code: Some(i32::from(!passed)),
            signal: None,
            duration_ms: 1_200,
            stdout: String::new(),
            stderr: stderr.to_owned(),
            timed_out: false,
        }
    }

    fn gate(kind: GateKind, passed: bool, stderr: &str) -> EventKind {
        EventKind::GateFinished {
            result: result(kind, passed, stderr),
        }
    }

    fn record(attempt: u32, reason: &str, gates: Vec<GateResult>) -> EventKind {
        EventKind::AttemptRecorded {
            record: Box::new(AttemptRecord {
                id: AttemptId::new(attempt),
                task: TaskId::new(1),
                started: datetime!(2024-01-15 10:00:00 UTC),
                ended: Some(datetime!(2024-01-15 10:05:03 UTC)),
                model_configured: Some("claude-opus-4".to_owned()),
                model_reported: Some("claude-opus-4-20250514".to_owned()),
                session_id: Some("sess-123".to_owned()),
                exit_reason: reason.to_owned(),
                gates,
                usage: Some(Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(50),
                    cached_tokens: None,
                    cost_usd: Some(0.05),
                    source: UsageSource::Provider,
                }),
                base_sha: "abc1234567890".to_owned(),
                candidate_sha: Some("def4567890123".to_owned()),
            }),
        }
    }

    /// The screen, one line per row, with trailing blanks removed.
    fn snapshot(app: &App) -> String {
        let harness = Harness::from_app(app.clone());
        let buffer = harness.buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < buffer.area.width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    // A wide symbol also covers the cell after it.
                    x += u16::try_from(display_width(symbol).max(1)).unwrap_or(1);
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn one_attempt() -> App {
        feed(
            app_with(vec![view(1, "Running")], (80, 24)),
            1,
            vec![
                started(1),
                entered(1, Phase::Red),
                entered(1, Phase::Green),
                entered(1, Phase::Refactor),
                gate(GateKind::Format, true, ""),
                gate(GateKind::Lint, true, ""),
                EventKind::GateStarted {
                    gate: GateKind::Build,
                },
            ],
        )
    }

    fn several_attempts() -> App {
        let failing = result(GateKind::Verify, false, "test result: FAILED. 1 failed");
        let passing: Vec<GateResult> = [
            GateKind::Format,
            GateKind::Lint,
            GateKind::Build,
            GateKind::Verify,
            GateKind::Privacy,
        ]
        .into_iter()
        .map(|kind| result(kind, true, ""))
        .collect();
        feed(
            app_with(vec![view(1, "Done")], (80, 24)),
            1,
            vec![
                started(1),
                entered(1, Phase::Red),
                entered(1, Phase::Green),
                entered(1, Phase::Refactor),
                entered(1, Phase::Verify),
                gate(GateKind::Format, true, ""),
                gate(GateKind::Verify, false, "test result: FAILED. 1 failed"),
                EventKind::VerifyFailed {
                    attempt: AttemptId::new(1),
                    class: FailureClass::VerificationFailure,
                    detail: "tests failed".to_owned(),
                },
                record(1, "verification failed", vec![failing]),
                EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
                entered(2, Phase::Green),
                entered(2, Phase::Verify),
                EventKind::VerifyPassed {
                    attempt: AttemptId::new(2),
                },
                entered(2, Phase::Publish),
                record(2, "completed", passing),
                EventKind::TaskDone {
                    commit: "def4567890123".to_owned(),
                },
            ],
        )
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn key(app: App, c: char) -> App {
        press(app, KeyCode::Char(c))
    }

    fn rows(app: &App) -> Vec<String> {
        snapshot(app).lines().map(str::to_owned).collect()
    }

    fn row_with(app: &App, needle: &str) -> Option<String> {
        rows(app).into_iter().find(|row| row.contains(needle))
    }

    /// The cell that holds the first character of `needle` on the screen.
    fn cell_of(app: &App, needle: &str) -> ratatui::buffer::Cell {
        let harness = Harness::from_app(app.clone());
        let buffer = harness.buffer();
        let width = buffer.area.width;
        for y in 0..buffer.area.height {
            let row: Vec<&str> = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            let text = row.concat();
            if let Some(at) = text.find(needle) {
                let column = text[..at].chars().count();
                return buffer[(u16::try_from(column).unwrap(), y)].clone();
            }
        }
        panic!("{needle:?} is not on the screen");
    }

    fn chips(app: &App) -> String {
        row_with(app, "Protocol").expect("the protocol row")
    }

    fn attempt_of(app: &App, task: u32, at: usize) -> &Attempt {
        app.inspector.histories[&TaskId::new(task)].nth(at).unwrap()
    }

    // ---- what the screen shows ----

    #[test]
    fn inspector_a_task_with_no_attempts_shows_its_definition_and_an_untouched_protocol() {
        let app = app_with(vec![view(1, "Queued")], (80, 24));
        let want = "\
5 Task inspector
 Task 1 · Task 1 title
 Queued · tdd · 0 attempts
Outcome    a task's full definition and history are visible
Done-when  snapshots cover a task with no attempts, one attempt, and several
Verify     cargo nextest run -p ktask-tui
Refs       VISION.md sections 9 and 13
Protocol   · red · green · refactor · verify · publish
           no phase entered yet
Gates      · format · lint · build · verify · privacy
Attempts   none yet
           No attempts yet.










[ / ] attempt  j / k task  g / G first / last
Press ? for the key map";
        assert_eq!(snapshot(&app), want);
    }

    #[test]
    fn inspector_one_attempt_shows_its_phase_gates_and_evidence_so_far() {
        let want = "\
5 Task inspector
 Task 1 · Task 1 title
 Running · tdd · 1 attempt
Outcome    a task's full definition and history are visible
Done-when  snapshots cover a task with no attempts, one attempt, and several
Verify     cargo nextest run -p ktask-tui
Refs       VISION.md sections 9 and 13
Protocol   ✓ red ✓ green ▶ refactor · verify · publish
           in refactor · may write any path · gate targeted
Gates      ✓ format ✓ lint ▶ build · verify · privacy
Attempts   1 of 1 · attempt 1 · newest
           verdict: none recorded
           gate format passed 1.2s
           gate lint passed 1.2s
           started 2024-01-15 10:00:00 · running
           commits: abc12345 → no candidate






[ / ] attempt  j / k task  g / G first / last
Press ? for the key map";
        assert_eq!(snapshot(&one_attempt()), want);
    }

    #[test]
    fn inspector_several_attempts_show_the_newest_first_and_the_older_on_request() {
        let newest = "\
5 Task inspector
 Task 1 · Task 1 title
 Done · tdd · 2 attempts
Outcome    a task's full definition and history are visible
Done-when  snapshots cover a task with no attempts, one attempt, and several
Verify     cargo nextest run -p ktask-tui
Refs       VISION.md sections 9 and 13
Protocol   ✓ red ✓ green ✓ refactor ✓ verify ✓ publish
           in publish · read-only · no gate · records evidence
Gates      ✓ format ✓ lint ✓ build ✓ verify ✓ privacy
Attempts   2 of 2 · attempt 2 (remediation) · newest
           verdict: passed
           gate format passed 1.2s
           gate lint passed 1.2s
           gate build passed 1.2s
           gate verify passed 1.2s
           gate privacy passed 1.2s
           started 2024-01-15 10:00:00 · ended 2024-01-15 10:05:03 · took 5m03s
           exit: completed
           model claude-opus-4-20250514 (configured claude-opus-4)
           session sess-123
           commits: abc12345 → def45678
[ / ] attempt  j / k task  g / G first / last
Press ? for the key map";
        let older = "\
5 Task inspector
 Task 1 · Task 1 title
 Done · tdd · 2 attempts
Outcome    a task's full definition and history are visible
Done-when  snapshots cover a task with no attempts, one attempt, and several
Verify     cargo nextest run -p ktask-tui
Refs       VISION.md sections 9 and 13
Protocol   ✓ red ✓ green ✓ refactor ✗ verify · publish
           in verify · read-only · gate verify · records evidence
Gates      · format · lint · build ✗ verify · privacy
Attempts   1 of 2 · attempt 1 · older
           verdict: failed · verification_failure: tests failed
           gate verify failed (exit 1) 1.2s
             └ test result: FAILED. 1 failed
           started 2024-01-15 10:00:00 · ended 2024-01-15 10:05:03 · took 5m03s
           exit: verification failed
           model claude-opus-4-20250514 (configured claude-opus-4)
           session sess-123
           commits: abc12345 → def45678
           usage: 100 in · 50 out · $0.05


[ / ] attempt  j / k task  g / G first / last
Press ? for the key map";
        let app = several_attempts();
        assert_eq!(snapshot(&app), newest);
        let app = key(app, '[');
        assert_eq!(snapshot(&app), older);
        assert_eq!(snapshot(&key(app, ']')), newest);
    }

    #[test]
    fn inspector_a_short_terminal_keeps_the_task_and_one_line_of_each_field() {
        let mut app = several_attempts();
        app.size = (40, 10);
        let want = "\
5 Task inspector
 Task 1 · Task 1 title
 Done · tdd · 2 attempts
Outcome    a task's full definition and…
Done-when  snapshots cover a task with…
Verify     cargo nextest run -p…
Refs       VISION.md sections 9 and 13
Protocol   ✓ red ✓ green ✓ refactor ✓ ve
Gates      ✓ format ✓ lint ✓ build ✓ ver
Attempts   2 of 2 · attempt 2 (remediati";
        assert_eq!(snapshot(&app), want);
    }

    #[test]
    fn inspector_without_tasks_says_there_is_nothing_to_inspect() {
        let app = app_with(Vec::new(), (40, 4));
        assert_eq!(snapshot(&app), "5 Task inspector\nNo task to inspect.\n\n");
    }

    #[test]
    fn inspector_a_task_whose_definition_was_never_given_says_so() {
        let app = App {
            screen: Screen::Inspector,
            tasks: vec![view(9, "Queued")],
            ..App::new((80, 24))
        };
        let row = row_with(&app, "Outcome").expect("an outcome row");
        assert_eq!(row, "Outcome    (the task's definition is not loaded)");
        assert!(row_with(&app, "Done-when").is_none_or(|row| row.trim_end() == "Done-when"));
    }

    // ---- the phase the journal says the task is in ----

    #[test]
    fn inspector_the_highlighted_phase_follows_the_last_phase_entered() {
        let mut app = feed(
            app_with(vec![view(1, "Running")], (80, 24)),
            1,
            vec![started(1)],
        );
        assert_eq!(app.inspector.current_phase(TaskId::new(1)), None);
        assert!(chips(&app).ends_with("· red · green · refactor · verify · publish"));
        let steps = [
            (Phase::Red, "▶ red · green · refactor · verify · publish"),
            (Phase::Green, "✓ red ▶ green · refactor · verify · publish"),
            (
                Phase::Refactor,
                "✓ red ✓ green ▶ refactor · verify · publish",
            ),
            (Phase::Verify, "✓ red ✓ green ✓ refactor ▶ verify · publish"),
            (
                Phase::Publish,
                "✓ red ✓ green ✓ refactor ✓ verify ▶ publish",
            ),
        ];
        for (phase, chips_after) in steps {
            app = feed(app, 1, vec![entered(1, phase)]);
            assert_eq!(app.inspector.current_phase(TaskId::new(1)), Some(phase));
            assert_eq!(
                chips(&app),
                format!("Protocol   {chips_after}"),
                "after {phase:?}"
            );
        }
    }

    #[test]
    fn inspector_only_the_current_phase_is_drawn_highlighted() {
        let app = one_attempt();
        let current = cell_of(&app, "▶ refactor");
        assert!(current.modifier.contains(Modifier::REVERSED));
        assert!(current.modifier.contains(Modifier::BOLD));
        for other in ["✓ red", "✓ green", "· verify", "· publish", "Protocol"] {
            let cell = cell_of(&app, other);
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "{other} is highlighted"
            );
        }
        assert_eq!(cell_of(&app, "✓ red").fg, Color::Green);
        assert!(cell_of(&app, "· verify").modifier.contains(Modifier::DIM));
    }

    #[test]
    fn inspector_the_phase_detail_names_what_the_current_phase_may_write_and_run() {
        let app = feed(one_attempt(), 1, vec![entered(1, Phase::Red)]);
        assert_eq!(
            row_with(&app, "in red").as_deref(),
            Some("           in red · may write tests only · gate targeted · records evidence")
        );
        let app = feed(app, 1, vec![entered(1, Phase::Verify)]);
        assert_eq!(
            row_with(&app, "in verify").as_deref(),
            Some("           in verify · read-only · gate verify · records evidence")
        );
        let app = feed(app, 1, vec![entered(1, Phase::Refactor)]);
        assert_eq!(
            row_with(&app, "in refactor").as_deref(),
            Some("           in refactor · may write any path · gate targeted")
        );
        let app = feed(app, 1, vec![entered(1, Phase::Publish)]);
        assert_eq!(
            row_with(&app, "in publish").as_deref(),
            Some("           in publish · read-only · no gate · records evidence")
        );
    }

    fn history_of(phases: &[Phase]) -> (History, Attempt) {
        let mut history = History::default();
        for phase in phases {
            history.apply(OffsetDateTime::UNIX_EPOCH, &entered(1, *phase));
        }
        let attempt = history
            .nth(0)
            .cloned()
            .unwrap_or_else(|| Attempt::new(AttemptId::new(1)));
        (history, attempt)
    }

    #[test]
    fn inspector_a_phase_is_pending_until_entered_done_once_left_and_current_while_running() {
        let (history, attempt) = history_of(&[Phase::Red, Phase::Green]);
        assert_eq!(history.mark(&attempt, true, Phase::Red), Mark::Done);
        assert_eq!(history.mark(&attempt, true, Phase::Green), Mark::Current);
        assert_eq!(history.mark(&attempt, true, Phase::Refactor), Mark::Pending);
    }

    #[test]
    fn inspector_the_last_phase_of_a_task_that_failed_or_stopped_is_stopped_not_current() {
        for end in [End::Failed, End::Cancelled, End::Interrupted] {
            let (mut history, attempt) = history_of(&[Phase::Red, Phase::Green]);
            history.end = Some(end);
            assert_eq!(history.mark(&attempt, true, Phase::Red), Mark::Done);
            assert_eq!(
                history.mark(&attempt, true, Phase::Green),
                Mark::Stopped,
                "{end:?}"
            );
        }
    }

    #[test]
    fn inspector_the_last_phase_of_a_finished_task_is_done() {
        let (mut history, attempt) = history_of(&[Phase::Verify, Phase::Publish]);
        history.end = Some(End::Done);
        assert_eq!(history.mark(&attempt, true, Phase::Publish), Mark::Done);
    }

    #[test]
    fn inspector_an_older_attempt_is_done_in_its_last_phase_unless_verification_rejected_it() {
        let (history, attempt) = history_of(&[Phase::Red, Phase::Green]);
        assert_eq!(history.mark(&attempt, false, Phase::Green), Mark::Done);
        let (mut history, mut attempt) = history_of(&[Phase::Red, Phase::Verify]);
        attempt.verdict = Some(Verdict::Failed {
            class: FailureClass::VerificationFailure,
            detail: String::new(),
        });
        assert_eq!(history.mark(&attempt, false, Phase::Verify), Mark::Stopped);
        history.end = None;
        assert_eq!(
            history.mark(&attempt, true, Phase::Verify),
            Mark::Stopped,
            "a rejected attempt is stopped even while the queue waits to remediate it"
        );
        assert_eq!(history.mark(&attempt, true, Phase::Red), Mark::Done);
    }

    #[test]
    fn inspector_a_declared_tdd_exception_marks_red_skipped_and_nothing_else() {
        let mut app = feed(
            app_with(vec![view(1, "Running")], (80, 24)),
            1,
            vec![
                started(1),
                EventKind::TddExceptionUsed {
                    exception: ktask_core::TddException::Documentation,
                    reason: "docs only".to_owned(),
                },
                entered(1, Phase::Green),
            ],
        );
        assert_eq!(
            chips(&app),
            "Protocol   ↷ red ▶ green · refactor · verify · publish"
        );
        app = feed(app, 1, vec![entered(1, Phase::Red)]);
        assert_eq!(
            chips(&app),
            "Protocol   ▶ red ✓ green · refactor · verify · publish",
            "a red that was entered is not skipped"
        );
    }

    #[test]
    fn inspector_a_remediation_round_carries_on_the_phases_an_earlier_attempt_finished() {
        let app = several_attempts();
        // Attempt 2 entered only green, verify and publish, yet red and
        // refactor were done in attempt 1 and the round carries on from them.
        assert_eq!(attempt_of(&app, 1, 1).phases.len(), 3);
        assert_eq!(
            chips(&app),
            "Protocol   ✓ red ✓ green ✓ refactor ✓ verify ✓ publish"
        );
        let history = &app.inspector.histories[&TaskId::new(1)];
        let first = history.nth(0).unwrap();
        let second = history.nth(1).unwrap();
        assert!(history.carried(second, Phase::Red));
        assert!(!history.carried(second, Phase::Publish));
        assert!(
            !history.carried(first, Phase::Red),
            "the first attempt carries on from nothing"
        );
    }

    #[test]
    fn inspector_a_phase_the_protocol_does_not_list_is_still_drawn_after_the_listed_ones() {
        let app = feed(
            one_attempt(),
            1,
            vec![entered(1, Phase::Review), entered(1, Phase::Harden)],
        );
        assert_eq!(
            chips(&app),
            "Protocol   ✓ red ✓ green ✓ refactor · verify · publish ✓ review ▶ harden"
        );
        assert_eq!(
            app.inspector.current_phase(TaskId::new(1)),
            Some(Phase::Harden)
        );
    }

    #[test]
    fn inspector_an_unknown_protocol_lists_only_the_phases_the_journal_shows() {
        let mut task_view = view(1, "Running");
        task_view.protocol = "spec-first".to_owned();
        let app = app_with(vec![task_view], (80, 24));
        assert_eq!(chips(&app), "Protocol   spec-first: no phases known");
        let app = feed(app, 1, vec![started(1), entered(1, Phase::Goal)]);
        assert_eq!(chips(&app), "Protocol   ▶ goal");
    }

    #[test]
    fn inspector_the_direct_protocol_lists_its_own_three_phases() {
        let mut task_view = view(1, "Running");
        task_view.protocol = "direct".to_owned();
        let app = feed(
            app_with(vec![task_view], (80, 24)),
            1,
            vec![started(1), entered(1, Phase::Implement)],
        );
        assert_eq!(chips(&app), "Protocol   ▶ implement · verify · publish");
    }

    #[test]
    fn inspector_the_protocol_falls_back_to_the_one_the_attempt_started_under() {
        let mut task_view = view(1, "Running");
        task_view.protocol = String::new();
        let app = feed(
            app_with(vec![task_view], (80, 24)),
            1,
            vec![started(1), entered(1, Phase::Red)],
        );
        assert_eq!(
            chips(&app),
            "Protocol   ▶ red · green · refactor · verify · publish"
        );
        assert!(row_with(&app, "Running").unwrap().contains("Running · tdd"));
    }

    #[test]
    fn inspector_no_protocol_at_all_is_shown_as_a_dash() {
        let mut task_view = view(1, "Queued");
        task_view.protocol = String::new();
        let app = app_with(vec![task_view], (80, 24));
        assert_eq!(
            row_with(&app, "Queued").as_deref(),
            Some(" Queued · - · 0 attempts")
        );
        assert_eq!(chips(&app), "Protocol   unknown: no phases known");
    }

    #[test]
    fn inspector_current_phase_is_none_once_the_task_has_ended() {
        let app = feed(
            one_attempt(),
            1,
            vec![EventKind::TaskDone { commit: "c".into() }],
        );
        assert_eq!(app.inspector.current_phase(TaskId::new(1)), None);
        assert_eq!(app.inspector.current_phase(TaskId::new(7)), None);
    }

    #[test]
    fn inspector_the_end_of_a_task_is_forgotten_when_work_begins_again() {
        let ends = [
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "x".into(),
            },
            EventKind::TaskCancelled { reason: "x".into() },
            EventKind::Interrupted {
                phase: Phase::Green,
            },
            EventKind::TaskDone { commit: "c".into() },
        ];
        let restarts = [
            EventKind::Resumed,
            EventKind::RetryStarted {
                attempt: AttemptId::new(9),
            },
            started(9),
        ];
        for end in ends {
            for restart in &restarts {
                let app = feed(one_attempt(), 1, vec![end.clone()]);
                assert!(app.inspector.histories[&TaskId::new(1)].end.is_some());
                let app = feed(app, 1, vec![restart.clone()]);
                assert_eq!(
                    app.inspector.histories[&TaskId::new(1)].end,
                    None,
                    "{end:?} then {restart:?}"
                );
            }
        }
    }

    #[test]
    fn inspector_each_way_a_task_can_end_is_recorded() {
        let cases = [
            (EventKind::TaskDone { commit: "c".into() }, End::Done),
            (
                EventKind::TaskFailed {
                    class: FailureClass::AgentFailure,
                    detail: "x".into(),
                },
                End::Failed,
            ),
            (
                EventKind::TaskCancelled { reason: "x".into() },
                End::Cancelled,
            ),
            (
                EventKind::Interrupted { phase: Phase::Red },
                End::Interrupted,
            ),
        ];
        for (kind, end) in cases {
            let app = feed(one_attempt(), 1, vec![kind]);
            let history = &app.inspector.histories[&TaskId::new(1)];
            assert_eq!(history.end, Some(end));
            assert_eq!(history.running_gate, None, "{end:?} ends the running gate");
        }
    }

    // ---- what the journal says about an attempt ----

    #[test]
    fn inspector_an_attempt_starts_with_its_protocol_base_and_time() {
        let app = feed(
            app_with(vec![view(1, "Running")], (80, 24)),
            1,
            vec![started(1)],
        );
        let attempt = attempt_of(&app, 1, 0);
        assert_eq!(attempt.id, AttemptId::new(1));
        assert_eq!(attempt.protocol.as_deref(), Some("tdd"));
        assert_eq!(attempt.base_sha.as_deref(), Some("abc1234567890"));
        assert_eq!(attempt.started, Some(datetime!(2024-01-15 10:00:00 UTC)));
        assert!(!attempt.remediation);
        assert_eq!(app.inspector.attempt_count(TaskId::new(1)), 1);
        assert_eq!(app.inspector.attempt_count(TaskId::new(2)), 0);
    }

    #[test]
    fn inspector_a_retry_starts_a_remediation_attempt() {
        let app = feed(
            one_attempt(),
            1,
            vec![EventKind::RetryStarted {
                attempt: AttemptId::new(2),
            }],
        );
        let attempt = attempt_of(&app, 1, 1);
        assert!(attempt.remediation);
        assert_eq!(attempt.started, Some(datetime!(2024-01-15 10:00:00 UTC)));
        assert!(!attempt_of(&app, 1, 0).remediation);
        assert_eq!(app.inspector.attempt_count(TaskId::new(1)), 2);
    }

    #[test]
    fn inspector_an_event_for_an_attempt_the_journal_window_missed_still_creates_it() {
        let app = feed(
            app_with(vec![view(1, "Running")], (80, 24)),
            1,
            vec![entered(4, Phase::Green)],
        );
        assert_eq!(attempt_of(&app, 1, 0).id, AttemptId::new(4));
        assert_eq!(
            chips(&app),
            "Protocol   ✓ red ▶ green · refactor · verify · publish".replace("✓ red", "· red")
        );
    }

    #[test]
    fn inspector_the_same_phase_entered_twice_is_one_phase() {
        let app = feed(
            one_attempt(),
            1,
            vec![entered(1, Phase::Green), entered(1, Phase::Green)],
        );
        assert_eq!(
            attempt_of(&app, 1, 0).phases,
            [Phase::Red, Phase::Green, Phase::Refactor]
        );
        assert_eq!(attempt_of(&app, 1, 0).current, Some(Phase::Green));
    }

    #[test]
    fn inspector_the_finished_attempt_records_its_exit_usage_session_and_model() {
        let app = feed(
            one_attempt(),
            1,
            vec![EventKind::AttemptFinished {
                attempt: AttemptId::new(1),
                exit_code: 3,
                usage: Some(Usage {
                    input_tokens: Some(7),
                    output_tokens: None,
                    cached_tokens: Some(2),
                    cost_usd: None,
                    source: UsageSource::Provider,
                }),
                session_id: Some("s-1".into()),
                model_reported: Some("opus".into()),
            }],
        );
        let attempt = attempt_of(&app, 1, 0);
        assert_eq!(attempt.exit_code, Some(3));
        assert_eq!(attempt.usage.as_deref(), Some("7 in · 2 cached"));
        assert_eq!(attempt.session_id.as_deref(), Some("s-1"));
        assert_eq!(attempt.model_reported.as_deref(), Some("opus"));
        assert!(row_with(&app, "exit: provider exit 3").is_some());
        assert!(row_with(&app, "usage: 7 in · 2 cached").is_some());
        assert!(row_with(&app, "model opus").is_some());
        assert!(row_with(&app, "session s-1").is_some());
    }

    #[test]
    fn inspector_the_verdict_is_recorded_for_the_attempt_it_names() {
        let app = feed(
            several_attempts(),
            1,
            vec![EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            }],
        );
        assert_eq!(attempt_of(&app, 1, 0).verdict, Some(Verdict::Passed));
        assert_eq!(attempt_of(&app, 1, 1).verdict, Some(Verdict::Passed));
        let app = feed(
            app,
            1,
            vec![EventKind::VerifyFailed {
                attempt: AttemptId::new(2),
                class: FailureClass::PolicyFailure,
                detail: "forbidden\nfile".into(),
            }],
        );
        assert_eq!(attempt_of(&app, 1, 0).verdict, Some(Verdict::Passed));
        assert_eq!(
            attempt_of(&app, 1, 1).verdict,
            Some(Verdict::Failed {
                class: FailureClass::PolicyFailure,
                detail: "forbidden ⏎ file".into(),
            })
        );
    }

    #[test]
    fn inspector_publishing_records_the_candidate_commit() {
        let app = feed(
            one_attempt(),
            1,
            vec![EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "0123456789abcdef".into(),
            }],
        );
        assert_eq!(
            attempt_of(&app, 1, 0).candidate_sha.as_deref(),
            Some("0123456789abcdef")
        );
        assert!(
            row_with(&app, "commits:")
                .unwrap()
                .ends_with("abc12345 → 01234567")
        );
    }

    #[test]
    fn inspector_the_recorded_evidence_overrides_what_the_attempts_events_said() {
        let app = feed(
            one_attempt(),
            1,
            vec![record(
                1,
                "completed",
                vec![result(GateKind::Lint, false, "boom")],
            )],
        );
        let attempt = attempt_of(&app, 1, 0);
        assert_eq!(
            attempt.gates.len(),
            1,
            "the record's gates replace the live ones"
        );
        assert_eq!(attempt.gates[0].line.kind, GateKind::Lint);
        assert_eq!(attempt.ended, Some(datetime!(2024-01-15 10:05:03 UTC)));
        assert_eq!(attempt.exit_reason.as_deref(), Some("completed"));
        assert_eq!(attempt.model_configured.as_deref(), Some("claude-opus-4"));
        assert_eq!(
            attempt.model_reported.as_deref(),
            Some("claude-opus-4-20250514")
        );
        assert_eq!(attempt.session_id.as_deref(), Some("sess-123"));
        assert_eq!(attempt.usage.as_deref(), Some("100 in · 50 out · $0.05"));
        assert_eq!(attempt.base_sha.as_deref(), Some("abc1234567890"));
        assert_eq!(attempt.candidate_sha.as_deref(), Some("def4567890123"));
        assert_eq!(app.inspector.attempt_count(TaskId::new(1)), 1);
    }

    #[test]
    fn inspector_a_gate_that_ran_before_any_attempt_belongs_to_none() {
        let app = feed(
            app_with(vec![view(1, "Preflight")], (80, 24)),
            1,
            vec![gate(GateKind::Baseline, true, "")],
        );
        assert_eq!(app.inspector.attempt_count(TaskId::new(1)), 0);
    }

    #[test]
    fn inspector_a_gate_belongs_to_the_newest_attempt() {
        let app = feed(
            several_attempts(),
            1,
            vec![gate(GateKind::Privacy, false, "leak")],
        );
        assert_eq!(attempt_of(&app, 1, 0).gates.len(), 1);
        assert_eq!(attempt_of(&app, 1, 1).gates.len(), 6);
    }

    #[test]
    fn inspector_a_failed_gate_shows_the_last_line_it_wrote_preferring_stderr() {
        let entry = |stdout: &str, stderr: &str| {
            let mut gate = result(GateKind::Lint, false, stderr);
            gate.stdout = stdout.to_owned();
            GateEntry::new(&gate).tail
        };
        assert_eq!(entry("ok\nwarn: a\n\n", "err 1\nerr 2\n\n"), "err 2");
        assert_eq!(entry("ok\nwarn: a\n\n", ""), "warn: a");
        assert_eq!(entry("", ""), "");
        assert_eq!(entry("", "  \n\x1b[31mred\x1b[0m  \n"), "red");
    }

    #[test]
    fn inspector_a_gate_that_is_running_is_marked_only_on_the_newest_attempt() {
        let app = one_attempt();
        assert_eq!(
            row_with(&app, "Gates").as_deref(),
            Some("Gates      ✓ format ✓ lint ▶ build · verify · privacy")
        );
        assert_eq!(
            app.inspector.histories[&TaskId::new(1)].running_gate,
            Some(GateKind::Build)
        );
        let app = feed(app, 1, vec![gate(GateKind::Build, true, "")]);
        assert_eq!(app.inspector.histories[&TaskId::new(1)].running_gate, None);
        assert_eq!(
            row_with(&app, "Gates").as_deref(),
            Some("Gates      ✓ format ✓ lint ✓ build · verify · privacy")
        );
        let app = feed(
            app,
            1,
            vec![
                EventKind::GateStarted {
                    gate: GateKind::Verify,
                },
                EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            ],
        );
        let older = key(app.clone(), '[');
        assert_eq!(
            row_with(&app, "Gates").as_deref(),
            Some("Gates      · format · lint · build · verify · privacy"),
            "the newer attempt has no results yet, and starting it stopped the gate"
        );
        assert_eq!(
            row_with(&older, "Gates").as_deref(),
            Some("Gates      ✓ format ✓ lint ✓ build · verify · privacy")
        );
    }

    #[test]
    fn inspector_a_running_gate_is_not_marked_on_an_older_attempt() {
        let mut app = feed(
            one_attempt(),
            1,
            vec![
                EventKind::GateStarted {
                    gate: GateKind::Verify,
                },
                EventKind::PhaseEntered {
                    attempt: AttemptId::new(2),
                    phase: Phase::Green,
                },
            ],
        );
        app = key(app, '[');
        assert_eq!(
            row_with(&app, "Gates").as_deref(),
            Some("Gates      ✓ format ✓ lint · build · verify · privacy")
        );
    }

    #[test]
    fn inspector_a_failed_or_timed_out_gate_is_marked_failed() {
        let mut timed_out = result(GateKind::Build, false, "");
        timed_out.timed_out = true;
        timed_out.exit_code = None;
        let app = feed(
            one_attempt(),
            1,
            vec![
                EventKind::GateFinished { result: timed_out },
                gate(GateKind::Format, false, "fmt"),
            ],
        );
        assert_eq!(
            row_with(&app, "Gates").as_deref(),
            Some("Gates      ✗ format ✓ lint ✗ build · verify · privacy"),
            "the last result of a gate is the one shown"
        );
        assert!(row_with(&app, "gate build timed out 1.2s").is_some());
        assert_eq!(cell_of(&app, "✗ build").fg, Color::Red);
    }

    #[test]
    fn inspector_the_completion_gates_can_be_set_to_those_configured() {
        let mut app = one_attempt();
        app.inspector
            .set_completion_gates(vec![GateKind::Lint, GateKind::Verify]);
        assert_eq!(
            row_with(&app, "Gates").as_deref(),
            Some("Gates      ✓ lint · verify")
        );
    }

    #[test]
    fn inspector_events_of_other_tasks_and_of_no_task_do_not_mix() {
        let app = feed(
            app_with(vec![view(1, "Running"), view(2, "Running")], (80, 24)),
            2,
            vec![started(1), entered(1, Phase::Red)],
        );
        let app = update(
            app,
            AppEvent::Core(Event {
                seq: EventSeq::new(2),
                ts: OffsetDateTime::UNIX_EPOCH,
                task_id: None,
                kind: entered(1, Phase::Green),
            }),
        );
        assert_eq!(app.inspector.attempt_count(TaskId::new(1)), 0);
        assert_eq!(app.inspector.attempt_count(TaskId::new(2)), 1);
        assert_eq!(
            app.inspector.current_phase(TaskId::new(2)),
            Some(Phase::Red)
        );
    }

    // ---- the evidence lines ----

    #[test]
    fn inspector_the_verdict_line_says_passed_failed_or_none() {
        let app = several_attempts();
        assert!(row_with(&app, "verdict: passed").is_some());
        assert_eq!(cell_of(&app, "verdict: passed").fg, Color::Green);
        let older = key(app, '[');
        assert_eq!(
            row_with(&older, "verdict").as_deref(),
            Some("           verdict: failed · verification_failure: tests failed")
        );
        assert_eq!(cell_of(&older, "verdict: failed").fg, Color::Red);
        assert!(row_with(&one_attempt(), "verdict: none recorded").is_some());
        assert!(
            cell_of(&one_attempt(), "verdict: none")
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn inspector_a_failed_gate_shows_its_output_below_it_and_a_passed_one_does_not() {
        let older = key(several_attempts(), '[');
        let rows = rows(&older);
        let at = rows
            .iter()
            .position(|r| r.contains("gate verify failed (exit 1) 1.2s"))
            .unwrap();
        assert_eq!(rows[at + 1], "             └ test result: FAILED. 1 failed");
        assert_eq!(cell_of(&older, "gate verify failed").fg, Color::Red);
        let newest = several_attempts();
        assert!(row_with(&newest, "└").is_none());
        assert_eq!(cell_of(&newest, "gate lint passed").fg, Color::Green);
    }

    #[test]
    fn inspector_timing_says_when_an_attempt_started_ended_and_how_long_it_took() {
        let app = several_attempts();
        assert_eq!(
            row_with(&app, "started").as_deref(),
            Some("           started 2024-01-15 10:00:00 · ended 2024-01-15 10:05:03 · took 5m03s")
        );
        assert_eq!(
            row_with(&one_attempt(), "started").as_deref(),
            Some("           started 2024-01-15 10:00:00 · running")
        );
        let ended = feed(
            one_attempt(),
            1,
            vec![EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "x".into(),
            }],
        );
        assert_eq!(
            row_with(&ended, "started").as_deref(),
            Some("           started 2024-01-15 10:00:00")
        );
        let bare = feed(
            app_with(vec![view(1, "Running")], (80, 24)),
            1,
            vec![entered(1, Phase::Red)],
        );
        assert_eq!(
            row_with(&bare, "running").as_deref(),
            Some("           running"),
            "an attempt the journal never showed starting is at least known to be running"
        );
        let older = key(
            feed(
                bare,
                1,
                vec![EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                }],
            ),
            '[',
        );
        assert_eq!(
            row_with(&older, "timing").as_deref(),
            Some("           timing: unknown")
        );
    }

    #[test]
    fn inspector_only_the_newest_running_attempt_is_called_running() {
        let app = feed(
            one_attempt(),
            1,
            vec![EventKind::RetryStarted {
                attempt: AttemptId::new(2),
            }],
        );
        let older = key(app.clone(), '[');
        assert!(row_with(&older, "running").is_none());
        assert!(row_with(&app, "running").is_some());
    }

    #[test]
    fn inspector_the_exit_line_joins_the_reason_and_the_providers_exit_code() {
        let app = feed(
            several_attempts(),
            1,
            vec![EventKind::AttemptFinished {
                attempt: AttemptId::new(2),
                exit_code: 0,
                usage: None,
                session_id: None,
                model_reported: None,
            }],
        );
        assert_eq!(
            row_with(&app, "exit:").as_deref(),
            Some("           exit: completed · provider exit 0")
        );
        assert_eq!(
            attempt_of(&app, 1, 1).usage,
            None,
            "an event without usage clears none it never had"
        );
        assert!(row_with(&one_attempt(), "exit:").is_none());
    }

    #[test]
    fn inspector_the_model_line_names_the_configured_model_only_when_it_differs() {
        let mut attempt = Attempt::new(AttemptId::new(1));
        assert!(model_lines(&attempt).is_empty());
        let text = |lines: Vec<Line<'static>>| -> Vec<String> {
            lines
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        attempt.model_configured = Some("a".into());
        assert_eq!(text(model_lines(&attempt)), ["model a"]);
        attempt.model_reported = Some("a".into());
        assert_eq!(text(model_lines(&attempt)), ["model a"]);
        attempt.model_reported = Some("b".into());
        assert_eq!(text(model_lines(&attempt)), ["model b (configured a)"]);
        attempt.model_configured = None;
        attempt.session_id = Some("s".into());
        assert_eq!(text(model_lines(&attempt)), ["model b", "session s"]);
        attempt.model_reported = None;
        assert_eq!(text(model_lines(&attempt)), ["session s"]);
    }

    #[test]
    fn inspector_the_commits_line_shortens_both_shas() {
        let app = several_attempts();
        assert_eq!(
            row_with(&app, "commits:").as_deref(),
            Some("           commits: abc12345 → def45678")
        );
        assert_eq!(
            row_with(&one_attempt(), "commits:").as_deref(),
            Some("           commits: abc12345 → no candidate")
        );
    }

    #[test]
    fn inspector_usage_is_described_by_the_figures_the_provider_gave() {
        let usage = |input, output, cached, cost| Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cost_usd: cost,
            source: UsageSource::Provider,
        };
        assert_eq!(
            describe_usage(&usage(Some(1), Some(2), Some(3), Some(0.5))),
            "1 in · 2 out · 3 cached · $0.50"
        );
        assert_eq!(describe_usage(&usage(None, Some(2), None, None)), "2 out");
        assert_eq!(
            describe_usage(&usage(None, None, None, Some(1.234))),
            "$1.23"
        );
        assert_eq!(
            describe_usage(&usage(None, None, None, None)),
            "not reported"
        );
    }

    // ---- moving between attempts and tasks ----

    #[test]
    fn inspector_brackets_step_through_the_attempts_and_stop_at_the_ends() {
        let mut app = several_attempts();
        let task = TaskId::new(1);
        assert_eq!(app.inspector.viewing(task), Some(1));
        app = key(app, ']');
        assert_eq!(app.inspector.viewing(task), Some(1), "already the newest");
        app = key(app, '[');
        assert_eq!(app.inspector.viewing(task), Some(0));
        app = key(app, '[');
        assert_eq!(app.inspector.viewing(task), Some(0), "already the first");
        app = key(app, ']');
        assert_eq!(app.inspector.viewing(task), Some(1));
        assert_eq!(app.inspector.viewing(TaskId::new(2)), None);
    }

    #[test]
    fn inspector_a_task_without_attempts_ignores_the_brackets() {
        let app = app_with(vec![view(1, "Queued")], (80, 24));
        let before = snapshot(&app);
        let app = key(key(app, '['), ']');
        assert_eq!(snapshot(&app), before);
        assert_eq!(app.inspector.viewing(TaskId::new(1)), None);
    }

    #[test]
    fn inspector_the_newest_attempt_is_followed_until_the_operator_steps_back() {
        let app = one_attempt();
        let app = feed(
            app,
            1,
            vec![EventKind::RetryStarted {
                attempt: AttemptId::new(2),
            }],
        );
        assert_eq!(
            app.inspector.viewing(TaskId::new(1)),
            Some(1),
            "following the newest"
        );
        let app = key(app, '[');
        let app = feed(
            app,
            1,
            vec![EventKind::RetryStarted {
                attempt: AttemptId::new(3),
            }],
        );
        assert_eq!(
            app.inspector.viewing(TaskId::new(1)),
            Some(0),
            "a pinned attempt stays shown as newer ones arrive"
        );
        let app = key(key(app, ']'), ']');
        assert_eq!(app.inspector.viewing(TaskId::new(1)), Some(2));
        assert_eq!(
            app.inspector.histories[&TaskId::new(1)].cursor,
            None,
            "following again"
        );
        let app = feed(
            app,
            1,
            vec![EventKind::RetryStarted {
                attempt: AttemptId::new(4),
            }],
        );
        assert_eq!(app.inspector.viewing(TaskId::new(1)), Some(3));
    }

    #[test]
    fn inspector_each_task_keeps_its_own_attempt() {
        let mut app = app_with(vec![view(1, "Done"), view(2, "Running")], (80, 24));
        app = feed(
            app,
            1,
            vec![
                started(1),
                EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            ],
        );
        app = feed(
            app,
            2,
            vec![
                started(1),
                EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            ],
        );
        app = key(app, '[');
        assert_eq!(app.inspector.viewing(TaskId::new(1)), Some(0));
        assert_eq!(app.inspector.viewing(TaskId::new(2)), Some(1));
        app = key(app, 'j');
        app = key(app, '[');
        assert_eq!(app.inspector.viewing(TaskId::new(1)), Some(0));
        assert_eq!(app.inspector.viewing(TaskId::new(2)), Some(0));
    }

    #[test]
    fn inspector_the_task_keys_move_between_tasks_and_stop_at_the_ends() {
        let mut app = app_with((1..=3).map(|id| view(id, "Queued")).collect(), (80, 24));
        let title = |app: &App| {
            rows(app)
                .into_iter()
                .find(|row| row.starts_with(" Task "))
                .unwrap()
        };
        assert_eq!(title(&app), " Task 1 · Task 1 title");
        app = key(app, 'j');
        assert_eq!(title(&app), " Task 2 · Task 2 title");
        app = press(app, KeyCode::Down);
        assert_eq!(title(&app), " Task 3 · Task 3 title");
        app = key(app, 'j');
        assert_eq!(title(&app), " Task 3 · Task 3 title", "stops at the last");
        app = key(app, 'k');
        assert_eq!(title(&app), " Task 2 · Task 2 title");
        app = press(app, KeyCode::Up);
        app = key(app, 'k');
        assert_eq!(title(&app), " Task 1 · Task 1 title", "stops at the first");
        app = key(app, 'G');
        assert_eq!(title(&app), " Task 3 · Task 3 title");
        app = key(app, 'g');
        assert_eq!(title(&app), " Task 1 · Task 1 title");
        assert_eq!(app.selected.get(&Screen::Inspector), Some(&0));
    }

    #[test]
    fn inspector_a_selection_past_the_end_of_the_queue_shows_the_last_task() {
        let mut app = app_with(vec![view(1, "Queued"), view(2, "Queued")], (80, 24));
        app.selected.insert(Screen::Inspector, 9);
        assert!(row_with(&app, " Task 2 ").is_some());
        let app = key(app, 'k');
        assert!(row_with(&app, " Task 1 ").is_some());
    }

    #[test]
    fn inspector_keys_do_nothing_on_other_screens_or_under_an_overlay() {
        let mut app = several_attempts();
        app.tasks.push(view(2, "Queued"));
        let before = app.inspector.clone();
        let mut elsewhere = app.clone();
        elsewhere.screen = Screen::Queue;
        let elsewhere = key(key(elsewhere, '['), ']');
        assert_eq!(elsewhere.inspector, before);
        let mut covered = app.clone();
        covered.overlay = Some(crate::types::Overlay::KeyMap);
        let covered = key(key(covered, '['), 'j');
        assert_eq!(covered.inspector, before);
        assert_eq!(covered.selected.get(&Screen::Inspector), None);
        let mut on_logs = app;
        on_logs.screen = Screen::Logs;
        assert_eq!(key(on_logs, 'j').selected.get(&Screen::Inspector), None);
    }

    #[test]
    fn inspector_an_unbound_key_changes_nothing_but_clears_the_notice() {
        let mut app = several_attempts();
        app.notice = Some("something".to_owned());
        let before = snapshot(&app);
        let app = key(app, 'z');
        assert_eq!(app.notice, None);
        assert_eq!(snapshot(&app), before);
    }

    #[test]
    fn inspector_an_empty_queue_ignores_every_key() {
        let app = app_with(Vec::new(), (80, 24));
        let app = key(key(key(app, 'j'), '['), 'G');
        assert!(app.selected.is_empty());
        assert_eq!(app.inspector.viewing(TaskId::new(1)), None);
    }

    #[test]
    fn inspector_opens_on_the_task_the_queue_selected() {
        let mut app = app_with(vec![view(1, "Done"), view(2, "Running")], (80, 24));
        app.screen = Screen::Queue;
        app = key(app, 'j');
        app = press(app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Inspector);
        assert!(row_with(&app, " Task 2 · Task 2 title").is_some());
        assert!(row_with(&app, "Running · tdd").is_some());
    }

    // ---- what reaches the screen ----

    #[test]
    fn inspector_the_definition_is_sanitized_and_wrapped_in_its_column() {
        let mut app = app_with(vec![view(1, "Queued")], (40, 24));
        let mut task = task(1);
        task.outcome =
            "one\x1b[31m red\x1b[0m\rtwo\x07 and a long line that must wrap in the pane".to_owned();
        app.inspector.set_definition(&task);
        let out = rows(&app);
        assert_eq!(out[3], "Outcome    one red");
        assert_eq!(out[4], "           two␦ and a long line that");
        assert_eq!(out[5], "           must wrap in the pane");
        assert!(out.iter().all(|row| !row.contains('\x1b')));
    }

    #[test]
    fn inspector_a_field_cut_short_ends_in_an_ellipsis() {
        let mut task = task(1);
        task.done_when = (1..=9)
            .map(|n| format!("criterion {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = app_with(vec![view(1, "Queued")], (80, 24));
        app.inspector.set_definition(&task);
        let out = rows(&app);
        let at = out.iter().position(|r| r.starts_with("Done-when")).unwrap();
        assert_eq!(out[at], "Done-when  criterion 1");
        assert_eq!(out[at + 1], "           criterion 2");
        assert_eq!(out[at + 2], "           criterion 3");
        assert_eq!(out[at + 3], "           criterion 4…");
        assert!(out[at + 4].starts_with("Verify"));
    }

    #[test]
    fn inspector_long_journal_text_is_cut_at_the_edge_of_the_pane() {
        let long = "x".repeat(500);
        let app = feed(
            several_attempts(),
            1,
            vec![EventKind::VerifyFailed {
                attempt: AttemptId::new(2),
                class: FailureClass::AgentFailure,
                detail: long,
            }],
        );
        let row = row_with(&app, "verdict").unwrap();
        assert_eq!(row.chars().count(), 80);
        assert!(row.starts_with("           verdict: failed · agent_failure: xxx"));
    }

    #[test]
    fn inspector_wide_characters_never_push_a_line_past_the_pane() {
        let mut task = task(1);
        task.outcome = "日本語のテキスト".repeat(8);
        let mut app = app_with(vec![view(1, "Queued")], (40, 24));
        app.inspector.set_definition(&task);
        let out = rows(&app);
        assert!(out[3].starts_with("Outcome    日本語"));
        for row in &out {
            assert!(display_width(row) <= 40, "{row:?}");
        }
    }

    #[test]
    fn inspector_draws_at_every_size_without_panicking_or_leaving_its_area() {
        let app = several_attempts();
        for width in [1, 2, 5, 10, 11, 12, 20, 30, 40, 79, 80, 81, 100u16] {
            for height in 1..=30u16 {
                let mut sized = app.clone();
                sized.size = (width, height);
                let text = Harness::from_app(sized).text();
                assert_eq!(
                    text.lines().count(),
                    usize::from(height),
                    "{width}x{height}"
                );
                for row in text.lines() {
                    assert!(display_width(row) <= usize::from(width), "{width}x{height}");
                }
            }
        }
    }

    #[test]
    fn inspector_the_hint_row_is_drawn_only_when_the_seven_fixed_lines_fit() {
        let app = several_attempts();
        assert_eq!(
            rows(&app).last().map(String::as_str),
            Some("Press ? for the key map")
        );
        assert!(row_with(&app, "[ / ] attempt").is_some());
        // A reduced layout has no footer: header, body of `height - 1`.
        let sized = |height| App {
            size: (80, height),
            ..app.clone()
        };
        let hint = |height| {
            rows(&sized(height))
                .into_iter()
                .rposition(|r| r.starts_with("[ / ]"))
        };
        assert_eq!(hint(12), Some(11), "the hint is the last row");
        assert_eq!(hint(11), None);
    }

    // ---- the helpers ----

    #[test]
    fn inspector_wrap_breaks_at_spaces_and_inside_words_only_when_it_must() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("aaaa bbbb", 6), ["aaaa", "bbbb"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("aa bb", 2), ["aa", "bb"]);
        assert_eq!(wrap("short", 40), ["short"]);
        assert_eq!(wrap("exact", 5), ["exact"]);
        assert_eq!(wrap("x", 0), ["x"]);
        assert!(wrap("", 10).is_empty());
    }

    #[test]
    fn inspector_wrap_keeps_line_breaks_drops_blank_lines_and_counts_columns() {
        assert_eq!(wrap("one\n\n  \n two  ", 10), ["one", " two"]);
        assert_eq!(wrap("日本語", 4), ["日本", "語"]);
        assert_eq!(wrap("a日b", 3), ["a日", "b"]);
    }

    #[test]
    fn inspector_share_deals_lines_out_one_at_a_time_in_order() {
        assert_eq!(share(&[2, 3], 4), [2, 2]);
        assert_eq!(share(&[1, 1, 1], 2), [1, 1, 0]);
        assert_eq!(share(&[1, 1], 5), [1, 1]);
        assert_eq!(share(&[3], 0), [0]);
        assert_eq!(share(&[0, 4], 3), [0, 3]);
        assert_eq!(share(&[4, 1, 1], 5), [3, 1, 1]);
        assert_eq!(share(&[], 5), Vec::<usize>::new());
    }

    #[test]
    fn inspector_durations_stamps_and_shas_are_written_short() {
        assert_eq!(duration(Duration::seconds(45)), "45s");
        assert_eq!(duration(Duration::seconds(60)), "1m00s");
        assert_eq!(duration(Duration::seconds(303)), "5m03s");
        assert_eq!(duration(Duration::seconds(3600)), "1h00m00s");
        assert_eq!(duration(Duration::seconds(3723)), "1h02m03s");
        assert_eq!(duration(Duration::seconds(-5)), "0s");
        assert_eq!(
            stamp(datetime!(2024-01-05 09:05:03 UTC)),
            "2024-01-05 09:05:03"
        );
        assert_eq!(short("abcdef1234567"), "abcdef12");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn inspector_clip_puts_text_on_one_line_and_bounds_its_length() {
        assert_eq!(clip("a\nb\n"), "a ⏎ b");
        assert_eq!(clip("\x1b[1mbold\x1b[0m"), "bold");
        let long = "x".repeat(MAX_TEXT_CHARS + 10);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(
            clip(&"y".repeat(MAX_TEXT_CHARS)).chars().count(),
            MAX_TEXT_CHARS
        );
        assert_eq!(clip_lines("a\nb\n"), "a\nb\n");
        assert_eq!(clip_lines(&long).chars().count(), MAX_TEXT_CHARS + 1);
    }

    #[test]
    fn inspector_a_line_is_fitted_span_by_span_to_the_width() {
        let line = Line::from(vec![
            Span::styled("abcd", Style::new().fg(Color::Red)),
            Span::raw("efgh"),
        ]);
        let fitted = fit(line.clone(), 6);
        let text: String = fitted.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abcdef");
        assert_eq!(fitted.spans[0].style.fg, Some(Color::Red));
        let none = fit(line.clone(), 0);
        assert!(none.spans.is_empty());
        let whole: String = fit(line, 8)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(whole, "abcdefgh");
    }

    #[test]
    fn inspector_every_mark_has_its_own_glyph_and_style() {
        let marks = [
            Mark::Done,
            Mark::Current,
            Mark::Stopped,
            Mark::Pending,
            Mark::Skipped,
        ];
        for (at, a) in marks.iter().enumerate() {
            for b in &marks[at + 1..] {
                assert_ne!(a.glyph(), b.glyph());
            }
        }
        assert_ne!(Mark::Done.style(), Mark::Stopped.style());
        assert_ne!(Mark::Done.style(), Mark::Pending.style());
        assert_eq!(Mark::Pending.style(), Mark::Skipped.style());
    }

    #[test]
    fn inspector_the_definition_can_be_replaced_and_is_kept_per_task() {
        let mut app = app_with(vec![view(1, "Queued"), view(2, "Queued")], (80, 24));
        let mut changed = task(1);
        changed.outcome = "something else".to_owned();
        app.inspector.set_definition(&changed);
        assert!(row_with(&app, "Outcome    something else").is_some());
        let app = key(app, 'j');
        assert!(row_with(&app, "Outcome    a task's full definition").is_some());
    }
}

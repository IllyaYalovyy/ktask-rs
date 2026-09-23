//! The failures screen: classified causes, repeated signatures, circuit-breaker
//! state and the actions available for each failure.
//!
//! [`FailureBoard`] folds the journal's failure events into a list of
//! [`Failure`]s, newest first. A failure is *classified* (a [`FailureClass`],
//! named as VISION.md §7 names it) and carries the failure *signature* the
//! core computes from that class and the failing gates the journal holds for
//! the attempt, so two failures that are the same failure have the same
//! signature. The board counts each signature per task, the way the runner's
//! [`Breaker`](ktask_core::Breaker) does, and a signature that has recurred
//! [`FailureBoard::threshold`] times has tripped the circuit breaker: no more
//! automatic attempts are made for that task until a human retries it, which
//! starts a fresh remediation and so a fresh count.
//!
//! There is no journal event for a trip, so the trip is derived, here, from
//! the counts. That keeps it consistent with the threshold in force even if
//! [`set_threshold`] changes it.
//!
//! A tripped breaker is the loudest thing on the screen: the first row is a
//! full-width bar, white on red, that says so and names the task and the
//! signature, and every failure row that hit the limit says `TRIPPED` in its
//! own cell. Nothing else on the screen is drawn on a red background, so it
//! cannot be mistaken for a class colour, and it survives a terminal without
//! colour because it is spelled out.
//!
//! Each of the nine classes has its own name and its own colour (see
//! [`class_name`] and [`class_style`]).
//!
//! What a failure allows depends on its class and on where its task now
//! stands (a task is `remediating`, `failed`, `cancelled` or `recovered`): a
//! failed task can be retried or cancelled, a failure of the gates or the
//! environment can have its gates re-run, and a `needs_input` failure needs an
//! answer, not another attempt, so it points at the input inbox instead. The
//! detail pane lists what is permitted; three of the operations have a key
//! (`r` retry, `c` cancel, `x` rerun-gate), which, as on the queue, append an
//! [`Action`] to [`App::outbox`] when permitted and otherwise say why not in
//! [`App::notice`].
//!
//! The selection is a cursor on a failure's number rather than on a row, so it
//! stays on the same failure while newer ones arrive; until it is moved it
//! follows the newest. Like every text that reaches the screen from the
//! journal, a failure's detail is sanitized before it is stored and cut at the
//! edge of the pane when it is drawn.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::LayoutPlan;
use crate::sanitize::sanitize;
use crate::text::{display_width, truncate_to_width};
use crate::types::{Action, Screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{AttemptId, Event, EventKind, FailureClass, GateResult, TaskId, signature};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::collections::{BTreeMap, VecDeque};

/// The most failures the board keeps; the oldest are dropped first.
pub const FAILURE_WINDOW: usize = 512;

/// The default number of identical signatures that trips the breaker, the
/// same as the core's `circuit_breaker_threshold`.
pub const DEFAULT_THRESHOLD: u32 = 3;

/// The longest text kept from a failure's detail, repairs or outcome.
const MAX_TEXT_CHARS: usize = 2_048;

/// What stands in for a line break in a text shown on one line.
const BREAK: &str = " ⏎ ";

/// What separates the entries of a detail line.
const SEPARATOR: &str = " · ";

/// What separates the entries of the action bar.
const BAR_GAP: &str = "  ";

/// The marker column that points at the selected row.
const MARKER: &str = "> ";

/// The marker column of a row that is not selected.
const NO_MARKER: &str = "  ";

/// What the list shows when there are no failures.
const EMPTY: &str = "No failures recorded.";

/// The fewest body rows that leave room for the action bar.
const BAR_MIN_HEIGHT: u16 = 5;

/// The fewest body rows that leave room for the detail pane as well: the
/// banner, a heading, six failures, the pane and the action bar.
const DETAIL_MIN_HEIGHT: u16 = 17;

/// The rows of the detail pane: a rule and up to seven lines.
const DETAIL_ROWS: u16 = 8;

/// The column headings, before the detail column, which takes what is left.
const HEADINGS: [&str; 7] = ["TASK", "ATT", "CLASS", "SIG", "REPEATS", "STATE", "DETAIL"];

/// The width each column before the detail asks for: wide enough for its
/// heading and for the longest value it can hold.
const WANTED: [usize; 6] = [4, 3, 22, SHORT_SIGNATURE, 12, 11];

/// How many hex digits of a signature the list shows.
const SHORT_SIGNATURE: usize = 8;

/// The name of `class` as VISION.md §7 gives it.
#[must_use]
pub fn class_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::AgentFailure => "agent_failure",
        FailureClass::VerificationFailure => "verification_failure",
        FailureClass::ProviderLimit => "provider_limit",
        FailureClass::ProviderTransient => "provider_transient",
        FailureClass::ProviderConfiguration => "provider_configuration",
        FailureClass::GitConflict => "git_conflict",
        FailureClass::EnvironmentFailure => "environment_failure",
        FailureClass::PolicyFailure => "policy_failure",
        FailureClass::NeedsInput => "needs_input",
    }
}

/// What `class` means, as VISION.md §7 says it.
#[must_use]
pub fn class_meaning(class: FailureClass) -> &'static str {
    match class {
        FailureClass::AgentFailure => "the agent could not complete the implementation",
        FailureClass::VerificationFailure => "tests, lint, build or privacy checks failed",
        FailureClass::ProviderLimit => "a usage limit, with a known or unknown reset",
        FailureClass::ProviderTransient => "a network error, temporary outage or process crash",
        FailureClass::ProviderConfiguration => {
            "authentication, invalid model or missing executable"
        }
        FailureClass::GitConflict => "branch drift, a rejected push or conflicting publication",
        FailureClass::EnvironmentFailure => "a missing SDK, dependency or host capability",
        FailureClass::PolicyFailure => "a forbidden file, a dirty tree or a gate bypass",
        FailureClass::NeedsInput => "an unresolved product or technical decision",
    }
}

/// The style of `class`'s name: a colour of its own for each class, so the
/// classes can be told apart at a glance as well as by name.
#[must_use]
pub fn class_style(class: FailureClass) -> Style {
    let style = Style::new();
    match class {
        FailureClass::AgentFailure => style.fg(Color::Red),
        FailureClass::VerificationFailure => style.fg(Color::Yellow),
        FailureClass::ProviderLimit => style.fg(Color::Magenta),
        FailureClass::ProviderTransient => style.fg(Color::Cyan),
        FailureClass::ProviderConfiguration => {
            style.fg(Color::LightMagenta).add_modifier(Modifier::BOLD)
        }
        FailureClass::GitConflict => style.fg(Color::Blue),
        FailureClass::EnvironmentFailure => style.fg(Color::LightBlue),
        FailureClass::PolicyFailure => style.fg(Color::LightRed).add_modifier(Modifier::BOLD),
        FailureClass::NeedsInput => style.fg(Color::Green),
    }
}

/// What VISION.md §7 says the supervisor does about `class` by itself.
fn policy(class: FailureClass) -> &'static str {
    match class {
        FailureClass::ProviderConfiguration | FailureClass::NeedsInput => {
            "never loops: it pauses for a human at once"
        }
        FailureClass::ProviderLimit => {
            "waits for the reset, with bounded backoff when it is unknown"
        }
        _ => "bounded by attempts, time and tokens; repeats trip the breaker",
    }
}

/// Which event recorded a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The preflight checks failed before an attempt began.
    Preflight,
    /// The completion gates of an attempt failed.
    Verification,
    /// The task failed with no verification failure before it to explain it.
    Task,
}

impl Stage {
    /// The stage's name as the detail pane shows it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Stage::Preflight => "preflight",
            Stage::Verification => "verification",
            Stage::Task => "task",
        }
    }
}

/// Where a task with a failure stands now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Standing {
    /// An attempt is being remediated.
    #[default]
    Remediating,
    /// The task failed and waits for a human to retry or cancel it.
    Failed,
    /// The task was cancelled.
    Cancelled,
    /// The task went on to complete.
    Recovered,
}

impl Standing {
    /// The standing's name as the list shows it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Standing::Remediating => "remediating",
            Standing::Failed => "failed",
            Standing::Cancelled => "cancelled",
            Standing::Recovered => "recovered",
        }
    }

    /// Whether the task still needs attention: it is being remediated or is
    /// failed, so a tripped breaker on it matters.
    fn is_live(self) -> bool {
        matches!(self, Standing::Remediating | Standing::Failed)
    }

    fn style(self) -> Style {
        match self {
            Standing::Remediating => Style::new().fg(Color::Magenta),
            Standing::Failed => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            Standing::Cancelled => Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT),
            Standing::Recovered => Style::new().fg(Color::Green),
        }
    }
}

/// What a remediation reported about a failure: the self-healing report of
/// VISION.md §7.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The repairs attempted, in the order they were tried.
    pub repairs: Vec<String>,
    /// How the remediation concluded.
    pub outcome: String,
}

/// One classified failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    number: u64,
    task: TaskId,
    attempt: Option<AttemptId>,
    stage: Stage,
    class: FailureClass,
    detail: String,
    signature: String,
    repeats: u32,
    report: Option<Report>,
}

impl Failure {
    /// The failure's number: how many failures the board had seen when this
    /// one arrived, counting it. Numbers only grow.
    #[must_use]
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The task that failed.
    #[must_use]
    pub fn task(&self) -> TaskId {
        self.task
    }

    /// The attempt that failed, when the failure names one.
    #[must_use]
    pub fn attempt(&self) -> Option<AttemptId> {
        self.attempt
    }

    /// Which event recorded the failure.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The failure's class.
    #[must_use]
    pub fn class(&self) -> FailureClass {
        self.class
    }

    /// The failure's description, sanitized and on one line.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The failure's signature: the core's [`signature`] of its class and the
    /// failing gates of its attempt.
    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// How many times this signature had been seen for the task, since its
    /// last retry, when this failure arrived, counting it.
    #[must_use]
    pub fn repeats(&self) -> u32 {
        self.repeats
    }

    /// The self-healing report of the remediation that answered this failure,
    /// once there is one.
    #[must_use]
    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }
}

/// A tripped circuit breaker: a task's signature that has recurred as often
/// as the threshold allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trip {
    /// The task whose breaker tripped.
    pub task: TaskId,
    /// The signature that recurred.
    pub signature: String,
    /// How many times it has recurred.
    pub count: u32,
}

/// What the board knows about one task's failures.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Track {
    standing: Standing,
    /// How often each signature has been seen since the last retry.
    counts: BTreeMap<String, u32>,
    /// The class of a verification failure that a `TaskFailed` of the same
    /// class may still be announcing, rather than a failure of its own.
    announced: Option<FailureClass>,
}

/// The failures the screen shows and the cursor on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureBoard {
    failures: VecDeque<Failure>,
    tracks: BTreeMap<TaskId, Track>,
    /// The failing gates of each task's latest attempt, which the failure
    /// signature is computed from.
    gates: BTreeMap<TaskId, Vec<GateResult>>,
    threshold: u32,
    added: u64,
    /// The number of the selected failure; `None` follows the newest.
    cursor: Option<u64>,
}

impl Default for FailureBoard {
    fn default() -> Self {
        Self {
            failures: VecDeque::new(),
            tracks: BTreeMap::new(),
            gates: BTreeMap::new(),
            threshold: DEFAULT_THRESHOLD,
            added: 0,
            cursor: None,
        }
    }
}

/// `text` sanitized, on one line and no longer than [`MAX_TEXT_CHARS`].
fn one_line(text: &str) -> String {
    let clean = sanitize(text);
    let joined = clean.trim_end_matches('\n').replace('\n', BREAK);
    match joined.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", joined.get(..end).unwrap_or_default()),
        None => joined,
    }
}

impl FailureBoard {
    /// How many failures the board holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.failures.len()
    }

    /// Whether the board holds no failure.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failures, newest first: the order the list shows them in.
    pub fn newest_first(&self) -> impl Iterator<Item = &Failure> {
        self.failures.iter().rev()
    }

    /// How many times a signature must recur to trip the breaker.
    #[must_use]
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Where `task` stands, if it has failed at all.
    #[must_use]
    pub fn standing(&self, task: TaskId) -> Option<Standing> {
        self.tracks.get(&task).map(|track| track.standing)
    }

    /// How many times `signature` has been seen for `task` since its last
    /// retry.
    #[must_use]
    pub fn count(&self, task: TaskId, signature: &str) -> u32 {
        self.tracks
            .get(&task)
            .and_then(|track| track.counts.get(signature))
            .copied()
            .unwrap_or(0)
    }

    /// The breakers that have tripped on tasks that still need attention,
    /// most repeated first, then by task.
    #[must_use]
    pub fn trips(&self) -> Vec<Trip> {
        let mut trips: Vec<Trip> = self
            .tracks
            .iter()
            .filter(|(_, track)| track.standing.is_live())
            .filter_map(|(task, track)| {
                track
                    .counts
                    .iter()
                    .filter(|(_, count)| **count >= self.threshold)
                    .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                    .map(|(signature, count)| Trip {
                        task: *task,
                        signature: signature.clone(),
                        count: *count,
                    })
            })
            .collect();
        trips.sort_by(|a, b| b.count.cmp(&a.count).then(a.task.cmp(&b.task)));
        trips
    }

    /// The most any signature has been seen on a task that still needs
    /// attention; zero when none has.
    fn most_repeated(&self) -> u32 {
        self.tracks
            .values()
            .filter(|track| track.standing.is_live())
            .flat_map(|track| track.counts.values().copied())
            .max()
            .unwrap_or(0)
    }

    /// The row of the selected failure in [`FailureBoard::newest_first`]
    /// order. A cursor whose failure has been dropped from the window rests on
    /// the oldest failure kept.
    fn position(&self) -> Option<usize> {
        let last = self.failures.len().checked_sub(1)?;
        Some(match self.cursor {
            None => 0,
            Some(number) => self
                .newest_first()
                .position(|failure| failure.number == number)
                .unwrap_or(last),
        })
    }

    /// The selected failure.
    #[must_use]
    pub fn selected(&self) -> Option<&Failure> {
        self.position().and_then(|at| self.newest_first().nth(at))
    }

    /// Moves the cursor to the `row`th failure, newest first. The first row
    /// is the newest failure, and the cursor on it follows the newest.
    fn select(&mut self, row: usize) {
        self.cursor = if row == 0 {
            None
        } else {
            self.newest_first().nth(row).map(|failure| failure.number)
        };
    }

    /// Adds a failure and counts its signature.
    fn record(
        &mut self,
        task: TaskId,
        attempt: Option<AttemptId>,
        stage: Stage,
        (class, detail): (FailureClass, &str),
        standing: Standing,
    ) {
        let gates = self.gates.get(&task).map_or(&[][..], Vec::as_slice);
        let signature = signature(class, gates);
        let track = self.tracks.entry(task).or_default();
        track.standing = standing;
        track.announced = (stage == Stage::Verification).then_some(class);
        let seen = track.counts.entry(signature.clone()).or_insert(0);
        *seen = seen.saturating_add(1);
        let repeats = *seen;
        self.added = self.added.saturating_add(1);
        self.failures.push_back(Failure {
            number: self.added,
            task,
            attempt,
            stage,
            class,
            detail: one_line(detail),
            signature,
            repeats,
            report: None,
        });
        while self.failures.len() > FAILURE_WINDOW {
            self.failures.pop_front();
        }
    }

    /// A task failure: the terminal echo of the verification failure just
    /// before it, if that was of the same class, otherwise a failure of its
    /// own.
    fn task_failed(&mut self, task: TaskId, class: FailureClass, detail: &str) {
        let echoed = self
            .tracks
            .get_mut(&task)
            .and_then(|track| track.announced.take())
            == Some(class);
        if echoed {
            if let Some(track) = self.tracks.get_mut(&task) {
                track.standing = Standing::Failed;
            }
        } else {
            self.record(task, None, Stage::Task, (class, detail), Standing::Failed);
        }
    }

    /// A human retry: a fresh remediation, and so a fresh breaker.
    fn retry(&mut self, task: TaskId) {
        self.gates.remove(&task);
        if let Some(track) = self.tracks.get_mut(&task) {
            track.counts.clear();
            track.announced = None;
            track.standing = Standing::Remediating;
        }
    }

    /// Attaches a self-healing report to the failure it answers: the task's
    /// latest failure on that attempt or of that class.
    fn attach(&mut self, task: TaskId, attempt: AttemptId, class: FailureClass, report: Report) {
        let target = self.failures.iter_mut().rev().find(|failure| {
            failure.task == task && (failure.attempt == Some(attempt) || failure.class == class)
        });
        if let Some(failure) = target {
            failure.report = Some(report);
        }
    }

    /// Notes that `task` has ended: cancelled, or done.
    fn settle(&mut self, task: TaskId, standing: Standing) {
        if let Some(track) = self.tracks.get_mut(&task) {
            track.standing = standing;
            track.announced = None;
        }
    }

    /// Folds one journal event into the board.
    fn apply(&mut self, event: &Event) {
        let Some(task) = event.task_id else {
            return;
        };
        match &event.kind {
            EventKind::AttemptStarted { .. } => {
                self.gates.remove(&task);
            }
            EventKind::GateFinished { result } if !result.passed => {
                self.gates.entry(task).or_default().push(result.clone());
            }
            EventKind::PreflightFailed { class, detail } => {
                let failure = (*class, detail.as_str());
                self.record(task, None, Stage::Preflight, failure, Standing::Failed);
            }
            EventKind::VerifyFailed {
                attempt,
                class,
                detail,
            } => {
                let failure = (*class, detail.as_str());
                let stage = Stage::Verification;
                self.record(task, Some(*attempt), stage, failure, Standing::Remediating);
            }
            EventKind::TaskFailed { class, detail } => self.task_failed(task, *class, detail),
            EventKind::RetryStarted { .. } => self.retry(task),
            EventKind::SelfHealingReport {
                attempt,
                class,
                repairs,
                outcome,
            } => {
                let report = Report {
                    repairs: repairs.iter().map(|repair| one_line(repair)).collect(),
                    outcome: one_line(outcome),
                };
                self.attach(task, *attempt, *class, report);
            }
            EventKind::TaskCancelled { .. } => self.settle(task, Standing::Cancelled),
            EventKind::TaskDone { .. } | EventKind::PublishVerified { .. } => {
                self.settle(task, Standing::Recovered);
            }
            _ => {}
        }
    }
}

/// Sets how many identical signatures trip the breaker.
pub fn set_threshold(app: &mut App, threshold: u32) {
    app.failures.threshold = threshold;
}

/// Folds one journal event into the failures.
pub fn fold(app: &mut App, event: &Event) {
    app.failures.apply(event);
}

/// The operations the screen offers on a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Retry,
    Cancel,
    RerunGate,
    Resolve,
}

impl Operation {
    /// The operations, in the order the detail pane lists them.
    const ALL: [Operation; 4] = [
        Operation::Retry,
        Operation::Cancel,
        Operation::RerunGate,
        Operation::Resolve,
    ];

    /// The operation `key` asks for. Modifiers unbind the key, so `Ctrl-C`
    /// stays quit and never cancels.
    fn from_key(key: &KeyEvent) -> Option<Self> {
        if key.modifiers != KeyModifiers::NONE {
            return None;
        }
        match key.code {
            KeyCode::Char('r') => Some(Operation::Retry),
            KeyCode::Char('c') => Some(Operation::Cancel),
            KeyCode::Char('x') => Some(Operation::RerunGate),
            _ => None,
        }
    }

    /// The key that asks for it; the answer to a question is typed in the
    /// input inbox, so `resolve` has none here.
    fn key(self) -> Option<&'static str> {
        match self {
            Operation::Retry => Some("r"),
            Operation::Cancel => Some("c"),
            Operation::RerunGate => Some("x"),
            Operation::Resolve => None,
        }
    }

    /// The CLI command's name.
    fn label(self) -> &'static str {
        match self {
            Operation::Retry => "retry",
            Operation::Cancel => "cancel",
            Operation::RerunGate => "rerun-gate",
            Operation::Resolve => "resolve",
        }
    }

    /// The action this operation carries out on `task`, if it is one the
    /// screen can ask for itself.
    fn action(self, task: TaskId) -> Option<Action> {
        match self {
            Operation::Retry => Some(Action::Retry { task }),
            Operation::Cancel => Some(Action::Cancel { task }),
            Operation::RerunGate => Some(Action::RerunGate { task, gate: None }),
            Operation::Resolve => None,
        }
    }
}

impl FailureBoard {
    /// Whether `operation` is permitted on `failure`, or the reason it is not,
    /// completing "task N ...".
    ///
    /// A failed task can be retried or cancelled, as the core's transitions
    /// say; a task still being remediated can only be cancelled; a finished
    /// one can do nothing. `needs_input` is answered, not retried, and only
    /// failures of the gates or of the environment have gates to re-run.
    fn permits(&self, failure: &Failure, operation: Operation) -> Result<(), String> {
        let standing = self.standing(failure.task).unwrap_or_default();
        let class = failure.class;
        match (operation, standing) {
            (_, Standing::Cancelled) => Err("is cancelled".to_owned()),
            (_, Standing::Recovered) => Err("has since completed".to_owned()),
            (Operation::Cancel, _) => Ok(()),
            (_, Standing::Remediating) => Err("is still being remediated".to_owned()),
            (Operation::Retry, Standing::Failed) if class == FailureClass::NeedsInput => Err(
                "needs an answer, not another attempt; resolve it in the input inbox (6)"
                    .to_owned(),
            ),
            (Operation::Resolve, Standing::Failed) if class != FailureClass::NeedsInput => {
                Err("asked no question to answer".to_owned())
            }
            (Operation::RerunGate, Standing::Failed)
                if !matches!(
                    class,
                    FailureClass::VerificationFailure | FailureClass::EnvironmentFailure
                ) =>
            {
                Err(format!(
                    "has no gates to re-run after {}",
                    class_name(class)
                ))
            }
            _ => Ok(()),
        }
    }
}

/// Carries out `operation` on the selected failure if it is permitted,
/// otherwise leaves only the reason in [`App::notice`].
fn perform(app: &mut App, operation: Operation) {
    let label = operation.label();
    let Some(failure) = app.failures.selected() else {
        app.notice = Some(format!("{label}: no failure is selected"));
        return;
    };
    let task = failure.task;
    match app.failures.permits(failure, operation) {
        Ok(()) => app.outbox.extend(operation.action(task)),
        Err(why) => app.notice = Some(format!("{label}: task {task} {why}")),
    }
}

/// Whether the failures screen has the keys: it is showing and nothing is over
/// it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::Failures && app.overlay.is_none()
}

/// Handles the failures' keys: `r`, `c` and `x` (see the module
/// documentation), and `j`, `k`, the arrows, `g` and `G`, which move the
/// selection down the list to older failures, up to newer ones, to the newest
/// and to the oldest. Does nothing on other screens, under an overlay, or for
/// other keys. Any key press here first clears the notice the last one left.
///
/// The selection stops at the ends rather than wrapping.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    app.notice = None;
    if let Some(operation) = Operation::from_key(key) {
        perform(app, operation);
        return;
    }
    let (Some(current), Some(last)) = (app.failures.position(), app.failures.len().checked_sub(1))
    else {
        return;
    };
    let next = match lookup(app.screen, key).map(|binding| binding.action) {
        Some(KeyAction::MoveDown) => (current + 1).min(last),
        Some(KeyAction::MoveUp) => current.saturating_sub(1),
        Some(KeyAction::First) => 0,
        Some(KeyAction::Last) => last,
        _ => return,
    };
    app.failures.select(next);
}

/// `text` cut to `width` columns and padded with spaces to exactly `width`.
fn cell(text: &str, width: usize) -> String {
    let cut = truncate_to_width(text, width);
    let padding = width.saturating_sub(display_width(&cut));
    format!("{cut}{}", " ".repeat(padding))
}

/// The widths of the columns when `total` columns are available: the columns
/// before the detail take what they ask for in order, those that no longer fit
/// get less, down to none, and the detail takes what is left.
fn column_widths(total: usize) -> ([usize; 6], usize) {
    let mut left = total.saturating_sub(MARKER.len());
    let widths = WANTED.map(|wanted| {
        let width = wanted.min(left);
        left = left.saturating_sub(width + 1);
        width
    });
    (widths, left)
}

/// The style of the cell that says a signature tripped the breaker.
fn tripped_style() -> Style {
    Style::new()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD)
}

/// One row of the list: the marker and the cells that got a width.
fn row_line(
    marker: &str,
    cells: [(String, Style); 7],
    (widths, detail): ([usize; 6], usize),
) -> Line<'static> {
    let mut spans = vec![Span::raw(marker.to_owned())];
    let all_widths = widths.into_iter().chain([detail]);
    let mut first = true;
    for ((text, style), width) in cells.into_iter().zip(all_widths) {
        if width == 0 {
            continue;
        }
        if !first {
            spans.push(Span::raw(" "));
        }
        first = false;
        spans.push(Span::styled(cell(&text, width), style));
    }
    Line::from(spans)
}

/// The list row of `failure`.
fn failure_line(
    board: &FailureBoard,
    failure: &Failure,
    selected: bool,
    widths: ([usize; 6], usize),
) -> Line<'static> {
    let plain = Style::new();
    let tripped = failure.repeats >= board.threshold;
    let repeats = format!("×{}/{}", failure.repeats, board.threshold);
    let (repeats, repeats_style) = if tripped {
        (format!("{repeats} TRIPPED"), tripped_style())
    } else {
        (repeats, plain)
    };
    let standing = board.standing(failure.task).unwrap_or_default();
    let cells = [
        (failure.task.to_string(), plain),
        (
            failure
                .attempt
                .map_or_else(|| "-".to_owned(), |attempt| attempt.to_string()),
            plain,
        ),
        (
            class_name(failure.class).to_owned(),
            class_style(failure.class),
        ),
        (
            failure
                .signature
                .chars()
                .take(SHORT_SIGNATURE)
                .collect::<String>(),
            plain,
        ),
        (repeats, repeats_style),
        (standing.label().to_owned(), standing.style()),
        (failure.detail.clone(), plain),
    ];
    let line = row_line(if selected { MARKER } else { NO_MARKER }, cells, widths);
    if selected {
        line.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

/// The first row: the circuit breaker's state. Tripped, it is a full-width bar
/// in [`tripped_style`] naming the worst trip; otherwise a plain line saying
/// the breaker is closed and how close the repeats have come.
fn banner(board: &FailureBoard) -> (String, Style) {
    let trips = board.trips();
    let Some(worst) = trips.first() else {
        let state = if board.is_empty() {
            "no failures".to_owned()
        } else {
            match board.most_repeated() {
                0 => "no repeats".to_owned(),
                most => format!("most repeated {most}× (trips at {})", board.threshold),
            }
        };
        return (
            format!("circuit breaker: closed{SEPARATOR}{state}"),
            Style::new().fg(Color::Green),
        );
    };
    let more = match trips.len() - 1 {
        0 => String::new(),
        others => format!(" (+{others} more)"),
    };
    let short: String = worst.signature.chars().take(SHORT_SIGNATURE).collect();
    let text = format!(
        "!! CIRCUIT BREAKER TRIPPED{SEPARATOR}task {}{more}{SEPARATOR}{short} ×{}/{}{SEPARATOR}no \
         more automatic retries",
        worst.task, worst.count, board.threshold
    );
    (text, tripped_style())
}

/// The permitted operations on `failure`, as the detail pane's last line
/// names them.
fn actions_line(board: &FailureBoard, failure: &Failure) -> String {
    let permitted: Vec<String> = Operation::ALL
        .into_iter()
        .filter(|operation| board.permits(failure, *operation).is_ok())
        .map(|operation| match operation.key() {
            Some(key) => format!("{} ({key})", operation.label()),
            None => format!("{} (input inbox, 6)", operation.label()),
        })
        .collect();
    if permitted.is_empty() {
        let standing = board.standing(failure.task).unwrap_or_default();
        format!(
            "actions: none{SEPARATOR}task {} is {}",
            failure.task,
            standing.label()
        )
    } else {
        format!("actions: {}", permitted.join(SEPARATOR))
    }
}

/// The lines of the detail pane for `failure`.
fn detail_lines(board: &FailureBoard, failure: &Failure, width: usize) -> Vec<Line<'static>> {
    let class = failure.class;
    let standing = board.standing(failure.task).unwrap_or_default();
    let attempt = failure
        .attempt
        .map_or_else(|| "-".to_owned(), |attempt| attempt.to_string());
    let seen = board.count(failure.task, &failure.signature);
    let breaker = if seen >= board.threshold {
        "breaker TRIPPED"
    } else {
        "breaker closed"
    };
    let mut lines = vec![
        Line::styled(
            format!("{} — {}", class_name(class), class_meaning(class)),
            class_style(class).add_modifier(Modifier::BOLD),
        ),
        Line::raw(format!(
            "task {}{SEPARATOR}attempt {attempt}{SEPARATOR}recorded by {}{SEPARATOR}{}",
            failure.task,
            failure.stage.label(),
            standing.label()
        )),
        Line::raw(format!(
            "signature {}{SEPARATOR}seen {seen}× of {}{SEPARATOR}{breaker}",
            failure.signature, board.threshold
        )),
        Line::raw(format!("detail: {}", failure.detail)),
        Line::raw(format!("policy: {}", policy(class))),
    ];
    if let Some(report) = &failure.report {
        lines.push(Line::raw(format!(
            "healing: {} → {}",
            report.repairs.join("; "),
            report.outcome
        )));
    }
    lines.push(Line::raw(actions_line(board, failure)));
    lines
        .into_iter()
        .map(|line| {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            Line::styled(truncate_to_width(&text, width), line.style)
        })
        .collect()
}

/// The action bar: each keyed operation and its key, dimmed when the selected
/// failure does not permit it, cut to `width` columns.
fn action_bar(board: &FailureBoard, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut left = width;
    let keyed = Operation::ALL
        .into_iter()
        .filter_map(|operation| operation.key().map(|key| (operation, key)));
    for (at, (operation, key)) in keyed.enumerate() {
        let allowed = board
            .selected()
            .is_some_and(|failure| board.permits(failure, operation).is_ok());
        let style = if allowed {
            Style::new()
        } else {
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)
        };
        let gap = if at == 0 { "" } else { BAR_GAP };
        let text = truncate_to_width(&format!("{gap}{key} {}", operation.label()), left);
        left -= display_width(&text);
        match text.strip_prefix(gap) {
            Some(entry) if !gap.is_empty() => {
                spans.push(Span::raw(gap));
                spans.push(Span::styled(entry.to_owned(), style));
            }
            _ => spans.push(Span::styled(text, style)),
        }
    }
    Line::from(spans)
}

/// Draws the failures into the body of `plan`: the circuit breaker's row, the
/// list, the selected failure's detail and the action bar, each where the body
/// is tall enough for it.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let board = &app.failures;
    let bar_rows = u16::from(board.selected().is_some() && body.height >= BAR_MIN_HEIGHT);
    let detail = if body.height >= DETAIL_MIN_HEIGHT && !board.is_empty() {
        DETAIL_ROWS
    } else {
        0
    };
    let [banner_area, list_area, detail_area, bar_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(detail),
        Constraint::Length(bar_rows),
    ])
    .areas(body);
    let (text, style) = banner(board);
    let text = truncate_to_width(&format!(" {text}"), width);
    frame.render_widget(Paragraph::new(text).style(style), banner_area);
    match board.selected() {
        None => frame.render_widget(
            Paragraph::new(truncate_to_width(EMPTY, width))
                .style(Style::new().add_modifier(Modifier::DIM)),
            list_area,
        ),
        Some(selected) => {
            render_list(board, list_area, frame);
            if detail > 0 {
                render_detail(board, selected, detail_area, frame);
            }
            frame.render_widget(Paragraph::new(action_bar(board, width)), bar_area);
        }
    }
    if let Some(notice) = &app.notice
        && body.height >= 2
    {
        let row = Rect {
            y: body.bottom() - 1,
            height: 1,
            ..body
        };
        frame.render_widget(Clear, row);
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to_width(notice, width),
                Style::new().fg(Color::Yellow),
            )),
            row,
        );
    }
}

/// Draws the heading and as many failures as fit, scrolled so the selected one
/// is in view.
fn render_list(board: &FailureBoard, area: Rect, frame: &mut Frame<'_>) {
    let Some(at) = board.position() else {
        return;
    };
    let widths = column_widths(usize::from(area.width));
    let headings = HEADINGS.map(|heading| (heading.to_owned(), Style::new()));
    let mut lines = vec![
        row_line(NO_MARKER, headings, widths).style(Style::new().add_modifier(Modifier::BOLD)),
    ];
    let rows = usize::from(area.height).saturating_sub(1);
    let offset = (at + 1).saturating_sub(rows);
    lines.extend(
        board
            .newest_first()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(row, failure)| failure_line(board, failure, row == at, widths)),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// Draws the rule and the detail of `failure`.
fn render_detail(board: &FailureBoard, failure: &Failure, area: Rect, frame: &mut Frame<'_>) {
    let width = usize::from(area.width);
    let mut lines = vec![Line::styled(
        "─".repeat(width),
        Style::new().add_modifier(Modifier::DIM),
    )];
    lines.extend(detail_lines(board, failure, width));
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::update;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::Overlay;
    use ktask_core::{Breaker, BreakerState, EventSeq, GateKind};
    use std::collections::HashSet;
    use time::OffsetDateTime;

    const CLASSES: [FailureClass; 9] = [
        FailureClass::AgentFailure,
        FailureClass::VerificationFailure,
        FailureClass::ProviderLimit,
        FailureClass::ProviderTransient,
        FailureClass::ProviderConfiguration,
        FailureClass::GitConflict,
        FailureClass::EnvironmentFailure,
        FailureClass::PolicyFailure,
        FailureClass::NeedsInput,
    ];

    fn event(task: u32, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(task)),
            kind,
        }
    }

    fn verify_failed(attempt: u32, class: FailureClass, detail: &str) -> EventKind {
        EventKind::VerifyFailed {
            attempt: AttemptId::new(attempt),
            class,
            detail: detail.into(),
        }
    }

    fn task_failed(class: FailureClass, detail: &str) -> EventKind {
        EventKind::TaskFailed {
            class,
            detail: detail.into(),
        }
    }

    fn attempt_started(attempt: u32) -> EventKind {
        EventKind::AttemptStarted {
            attempt: AttemptId::new(attempt),
            protocol: "tdd".into(),
            pid: 7,
            base_sha: "abc".into(),
        }
    }

    fn gate(passed: bool, stdout: &str) -> EventKind {
        EventKind::GateFinished {
            result: GateResult {
                kind: GateKind::Verify,
                passed,
                exit_code: Some(i32::from(!passed)),
                signal: None,
                duration_ms: 5,
                stdout: stdout.into(),
                stderr: String::new(),
                timed_out: false,
            },
        }
    }

    fn cargo_output(failing: &str, finished_in: &str) -> String {
        format!(
            "running 1 test\ntest {failing} ... FAILED\n\nfailures:\n\n---- {failing} stdout ----\n\
             assertion failed\n\nfailures:\n    {failing}\n\n\
             test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in {finished_in}\n"
        )
    }

    fn app_at(size: (u16, u16), events: Vec<(u32, EventKind)>) -> App {
        let app = App {
            screen: Screen::Failures,
            ..App::new(size)
        };
        events.into_iter().fold(app, |app, (task, kind)| {
            update(app, AppEvent::Core(event(task, kind)))
        })
    }

    fn app_with(events: Vec<(u32, EventKind)>) -> App {
        app_at((80, 24), events)
    }

    /// `count` failures of `class` on `task`, each on its own attempt.
    fn repeated(task: u32, class: FailureClass, count: u32) -> Vec<(u32, EventKind)> {
        (1..=count)
            .map(|attempt| (task, verify_failed(attempt, class, "tests failed")))
            .collect()
    }

    fn rows(harness: &Harness) -> Vec<String> {
        harness.text().lines().map(str::to_owned).collect()
    }

    fn row_starting(harness: &Harness, prefix: &str) -> Option<String> {
        rows(harness)
            .into_iter()
            .find(|row| row.trim_start().starts_with(prefix))
    }

    fn short(class: FailureClass) -> String {
        signature(class, &[])
            .chars()
            .take(SHORT_SIGNATURE)
            .collect()
    }

    fn press(harness: &mut Harness, code: KeyCode) {
        harness.send(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn selected_task(app: &App) -> Option<u32> {
        app.failures.selected().map(|failure| failure.task().get())
    }

    // ---- what the board folds out of the journal ----

    #[test]
    fn failures_a_verification_failure_is_listed_with_its_class_task_attempt_and_detail() {
        let app = app_with(vec![(
            3,
            verify_failed(2, FailureClass::VerificationFailure, "tests failed"),
        )]);
        let failure = app.failures.selected().expect("a failure");
        assert_eq!(failure.task(), TaskId::new(3));
        assert_eq!(failure.attempt(), Some(AttemptId::new(2)));
        assert_eq!(failure.class(), FailureClass::VerificationFailure);
        assert_eq!(failure.stage(), Stage::Verification);
        assert_eq!(failure.detail(), "tests failed");
        assert_eq!(failure.repeats(), 1);
        assert_eq!(failure.report(), None);
        assert_eq!(
            app.failures.standing(TaskId::new(3)),
            Some(Standing::Remediating)
        );
        assert_eq!(app.failures.len(), 1);
    }

    #[test]
    fn failures_a_preflight_failure_is_listed_and_leaves_the_task_failed() {
        let app = app_with(vec![(
            2,
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "disk full".into(),
            },
        )]);
        let failure = app.failures.selected().expect("a failure");
        assert_eq!(failure.stage(), Stage::Preflight);
        assert_eq!(failure.attempt(), None);
        assert_eq!(failure.class(), FailureClass::EnvironmentFailure);
        assert_eq!(
            app.failures.standing(TaskId::new(2)),
            Some(Standing::Failed)
        );
    }

    #[test]
    fn failures_the_task_failure_that_follows_its_verification_failure_is_the_same_failure() {
        let class = FailureClass::VerificationFailure;
        let app = app_with(vec![
            (3, verify_failed(1, class, "tests failed")),
            (3, task_failed(class, "gave up")),
        ]);
        assert_eq!(app.failures.len(), 1);
        assert_eq!(app.failures.selected().map(Failure::repeats), Some(1));
        assert_eq!(
            app.failures.standing(TaskId::new(3)),
            Some(Standing::Failed)
        );
    }

    #[test]
    fn failures_a_task_failure_of_another_class_is_a_failure_of_its_own() {
        let app = app_with(vec![
            (
                3,
                verify_failed(1, FailureClass::VerificationFailure, "tests failed"),
            ),
            (3, task_failed(FailureClass::AgentFailure, "agent crashed")),
        ]);
        assert_eq!(app.failures.len(), 2);
        let newest = app.failures.selected().expect("a failure");
        assert_eq!(newest.class(), FailureClass::AgentFailure);
        assert_eq!(newest.stage(), Stage::Task);
        assert_eq!(newest.attempt(), None);
        assert_eq!(newest.detail(), "agent crashed");
        assert_eq!(
            app.failures.standing(TaskId::new(3)),
            Some(Standing::Failed)
        );
    }

    #[test]
    fn failures_only_one_task_failure_echoes_a_verification_failure() {
        let class = FailureClass::VerificationFailure;
        let app = app_with(vec![
            (3, verify_failed(1, class, "tests failed")),
            (3, task_failed(class, "gave up")),
            (3, task_failed(class, "gave up again")),
        ]);
        assert_eq!(app.failures.len(), 2);
    }

    #[test]
    fn failures_a_task_failure_after_a_retry_is_not_an_echo_of_an_older_failure() {
        let class = FailureClass::VerificationFailure;
        let app = app_with(vec![
            (3, verify_failed(1, class, "tests failed")),
            (
                3,
                EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            ),
            (3, task_failed(class, "gave up")),
        ]);
        assert_eq!(app.failures.len(), 2);
    }

    #[test]
    fn failures_identical_signatures_are_counted_per_task_and_per_class() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        events.extend(repeated(4, class, 1));
        events.push((3, verify_failed(4, FailureClass::AgentFailure, "crashed")));
        let app = app_with(events);
        let counted: Vec<(u32, u32)> = app
            .failures
            .newest_first()
            .map(|failure| (failure.task().get(), failure.repeats()))
            .collect();
        assert_eq!(counted, [(3, 1), (4, 1), (3, 3), (3, 2), (3, 1)]);
        let sig = signature(class, &[]);
        assert_eq!(app.failures.count(TaskId::new(3), &sig), 3);
        assert_eq!(app.failures.count(TaskId::new(4), &sig), 1);
        assert_eq!(app.failures.count(TaskId::new(9), &sig), 0);
    }

    #[test]
    fn failures_the_signature_is_the_cores_over_the_failing_gates_of_the_attempt() {
        let class = FailureClass::VerificationFailure;
        let output = cargo_output("tests::a", "0.1s");
        let app = app_with(vec![
            (3, gate(false, &output)),
            (3, verify_failed(1, class, "tests failed")),
        ]);
        let expected = signature(
            class,
            &[GateResult {
                kind: GateKind::Verify,
                passed: false,
                exit_code: Some(1),
                signal: None,
                duration_ms: 5,
                stdout: output,
                stderr: String::new(),
                timed_out: false,
            }],
        );
        let failure = app.failures.selected().expect("a failure");
        assert_eq!(failure.signature(), expected);
        assert_ne!(failure.signature(), signature(class, &[]));
    }

    #[test]
    fn failures_the_same_failing_test_repeats_however_long_it_took() {
        let class = FailureClass::VerificationFailure;
        let app = app_with(vec![
            (3, gate(false, &cargo_output("tests::a", "0.1s"))),
            (3, verify_failed(1, class, "tests failed")),
            (3, attempt_started(2)),
            (3, gate(false, &cargo_output("tests::a", "9.9s"))),
            (3, verify_failed(2, class, "tests failed")),
            (3, attempt_started(3)),
            (3, gate(false, &cargo_output("tests::b", "0.1s"))),
            (3, verify_failed(3, class, "tests failed")),
        ]);
        let repeats: Vec<u32> = app.failures.newest_first().map(Failure::repeats).collect();
        assert_eq!(repeats, [1, 2, 1]);
    }

    #[test]
    fn failures_a_passing_gate_does_not_change_the_signature() {
        let class = FailureClass::VerificationFailure;
        let app = app_with(vec![
            (3, gate(true, &cargo_output("tests::a", "0.1s"))),
            (3, verify_failed(1, class, "tests failed")),
        ]);
        let failure = app.failures.selected().expect("a failure");
        assert_eq!(failure.signature(), signature(class, &[]));
    }

    #[test]
    fn failures_a_retry_gives_the_task_a_fresh_count_and_clears_its_trip() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        let tripped = app_with(events.clone());
        assert_eq!(tripped.failures.trips().len(), 1);
        events.push((
            3,
            EventKind::RetryStarted {
                attempt: AttemptId::new(4),
            },
        ));
        let retried = app_with(events.clone());
        assert!(retried.failures.trips().is_empty());
        assert_eq!(
            retried
                .failures
                .count(TaskId::new(3), &signature(class, &[])),
            0
        );
        assert_eq!(
            retried.failures.standing(TaskId::new(3)),
            Some(Standing::Remediating)
        );
        assert_eq!(retried.failures.len(), 3);
        events.push((3, verify_failed(4, class, "tests failed")));
        let again = app_with(events);
        assert_eq!(again.failures.selected().map(Failure::repeats), Some(1));
    }

    #[test]
    fn failures_cancelling_or_finishing_a_task_settles_it_and_clears_its_trip() {
        let class = FailureClass::VerificationFailure;
        for (event, standing) in [
            (
                EventKind::TaskCancelled {
                    reason: "no".into(),
                },
                Standing::Cancelled,
            ),
            (
                EventKind::TaskDone {
                    commit: "abc".into(),
                },
                Standing::Recovered,
            ),
            (
                EventKind::PublishVerified {
                    commit: "abc".into(),
                    remote_sha: "abc".into(),
                },
                Standing::Recovered,
            ),
        ] {
            let mut events = repeated(3, class, 3);
            events.push((3, event));
            let app = app_with(events);
            assert_eq!(app.failures.standing(TaskId::new(3)), Some(standing));
            assert!(app.failures.trips().is_empty(), "{standing:?}");
        }
    }

    #[test]
    fn failures_a_task_that_never_failed_is_not_tracked() {
        let app = app_with(vec![(
            9,
            EventKind::TaskDone {
                commit: "abc".into(),
            },
        )]);
        assert_eq!(app.failures.standing(TaskId::new(9)), None);
        assert!(app.failures.is_empty());
    }

    #[test]
    fn failures_a_self_healing_report_is_attached_to_the_failure_it_answers() {
        let class = FailureClass::VerificationFailure;
        let report = EventKind::SelfHealingReport {
            attempt: AttemptId::new(1),
            class,
            repairs: vec!["fixed the test".into(), "\x1b[31mrebuilt\x1b[0m".into()],
            outcome: "verification passed on retry".into(),
        };
        let app = app_with(vec![
            (3, verify_failed(1, class, "tests failed")),
            (4, verify_failed(1, class, "other task")),
            (3, report),
        ]);
        let failures: Vec<&Failure> = app.failures.newest_first().collect();
        assert_eq!(failures[0].report(), None, "task 4's failure");
        let attached = failures[1].report().expect("task 3's failure has it");
        assert_eq!(attached.repairs, ["fixed the test", "rebuilt"]);
        assert_eq!(attached.outcome, "verification passed on retry");
    }

    #[test]
    fn failures_a_self_healing_report_with_nothing_to_answer_changes_nothing() {
        let report = EventKind::SelfHealingReport {
            attempt: AttemptId::new(1),
            class: FailureClass::AgentFailure,
            repairs: Vec::new(),
            outcome: "nothing".into(),
        };
        let app = app_with(vec![(3, report)]);
        assert!(app.failures.is_empty());
    }

    #[test]
    fn failures_events_that_name_no_task_or_no_failure_change_nothing() {
        let before = app_with(repeated(3, FailureClass::AgentFailure, 2));
        let mut anonymous = event(3, verify_failed(9, FailureClass::AgentFailure, "x"));
        anonymous.task_id = None;
        let after = update(before.clone(), AppEvent::Core(anonymous));
        assert_eq!(after.failures, before.failures);
        let after = update(before.clone(), AppEvent::Core(event(3, EventKind::Resumed)));
        assert_eq!(after.failures, before.failures);
    }

    #[test]
    fn failures_the_breaker_trips_exactly_when_the_cores_breaker_does() {
        let class = FailureClass::VerificationFailure;
        for threshold in 1..=4 {
            for count in 1..=5 {
                let mut app = app_with(repeated(3, class, count));
                set_threshold(&mut app, threshold);
                let mut breaker = Breaker::new(threshold);
                let sig = signature(class, &[]);
                let mut state = BreakerState::Closed { count: 0 };
                for _ in 0..count {
                    state = breaker.record(&sig);
                }
                let tripped = matches!(state, BreakerState::Tripped { .. });
                assert_eq!(
                    !app.failures.trips().is_empty(),
                    tripped,
                    "{count} failures against {threshold}"
                );
            }
        }
    }

    #[test]
    fn failures_the_threshold_starts_at_the_cores_default_and_can_be_set() {
        let mut app = App::new((80, 24));
        assert_eq!(app.failures.threshold(), 3);
        assert_eq!(
            DEFAULT_THRESHOLD,
            ktask_core::Config::default().circuit_breaker_threshold
        );
        set_threshold(&mut app, 5);
        assert_eq!(app.failures.threshold(), 5);
    }

    #[test]
    fn failures_a_trip_names_the_most_repeated_signature_of_each_live_task() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        events.extend(repeated(5, class, 4));
        events.extend(repeated(7, class, 2));
        let app = app_with(events);
        let sig = signature(class, &[]);
        assert_eq!(
            app.failures.trips(),
            [
                Trip {
                    task: TaskId::new(5),
                    signature: sig.clone(),
                    count: 4
                },
                Trip {
                    task: TaskId::new(3),
                    signature: sig,
                    count: 3
                },
            ]
        );
    }

    #[test]
    fn failures_the_board_keeps_a_window_and_the_cursor_rests_on_the_oldest_when_dropped() {
        let mut app = app_with(Vec::new());
        for task in 1..=u32::try_from(FAILURE_WINDOW).expect("small") {
            app = update(
                app,
                AppEvent::Core(event(task, task_failed(FailureClass::AgentFailure, "x"))),
            );
        }
        let mut harness = Harness::from_app(app);
        press(&mut harness, KeyCode::Char('G'));
        assert_eq!(selected_task(harness.app()), Some(1));
        let app = update(
            harness.app().clone(),
            AppEvent::Core(event(900, task_failed(FailureClass::AgentFailure, "x"))),
        );
        assert_eq!(app.failures.len(), FAILURE_WINDOW);
        assert_eq!(
            app.failures.newest_first().next().map(Failure::number),
            Some(513)
        );
        assert_eq!(selected_task(&app), Some(2));
    }

    #[test]
    fn failures_a_long_or_hostile_detail_is_kept_safe_and_short() {
        let hostile = format!(
            "\x1b[31mred\x1b[0m\x1b]0;pwned\x07\nsecond\rthird{}",
            "z".repeat(5_000)
        );
        let app = app_with(vec![(3, task_failed(FailureClass::AgentFailure, &hostile))]);
        let detail = app
            .failures
            .selected()
            .expect("a failure")
            .detail()
            .to_owned();
        assert!(detail.starts_with("red ⏎ second"), "{detail:?}");
        assert!(!detail.contains(['\x1b', '\r', '\n', '\x07']));
        assert_eq!(detail.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(detail.ends_with('…'));
        let harness = Harness::from_app(app);
        assert!(!harness.text().contains(['\x1b', '\x07']));
    }

    // ---- what the screen draws ----

    #[test]
    fn failures_the_empty_screen_says_the_breaker_is_closed_and_nothing_failed() {
        let harness = Harness::from_app(app_with(Vec::new()));
        let rows = rows(&harness);
        assert_eq!(rows[0].trim_end(), "4 Failures");
        assert_eq!(rows[1].trim_end(), " circuit breaker: closed · no failures");
        assert_eq!(rows[2].trim_end(), "No failures recorded.");
        assert_eq!(rows[3].trim(), "");
    }

    #[test]
    fn failures_the_class_names_are_the_ones_vision_gives() {
        let names: Vec<&str> = CLASSES.into_iter().map(class_name).collect();
        assert_eq!(
            names,
            [
                "agent_failure",
                "verification_failure",
                "provider_limit",
                "provider_transient",
                "provider_configuration",
                "git_conflict",
                "environment_failure",
                "policy_failure",
                "needs_input",
            ]
        );
    }

    #[test]
    fn failures_every_class_has_its_own_name_meaning_and_style() {
        let meanings: HashSet<&str> = CLASSES.into_iter().map(class_meaning).collect();
        assert_eq!(meanings.len(), CLASSES.len());
        assert!(meanings.iter().all(|meaning| !meaning.trim().is_empty()));
        for (at, class) in CLASSES.into_iter().enumerate() {
            for other in CLASSES.into_iter().skip(at + 1) {
                assert_ne!(
                    class_style(class),
                    class_style(other),
                    "{class:?} {other:?}"
                );
            }
            assert_eq!(
                class_style(class).bg,
                None,
                "{class:?} must not look tripped"
            );
        }
    }

    #[test]
    fn failures_every_class_renders_distinctly_in_the_list_and_the_detail() {
        let mut screens = HashSet::new();
        for class in CLASSES {
            let harness = Harness::from_app(app_with(vec![(3, task_failed(class, "boom"))]));
            let rows = rows(&harness);
            let name = class_name(class);
            assert!(rows[3].contains(name), "{class:?}: {}", rows[3]);
            let x = rows[3].find(name).expect("named in the list");
            let x = u16::try_from(rows[3][..x].chars().count()).expect("narrow");
            let cell = &harness.buffer()[(x, 3)];
            assert_eq!(cell.style().fg, class_style(class).fg, "{class:?}");
            let detail = row_starting(&harness, name).map(|row| row.trim_end().to_owned());
            assert!(detail.is_some());
            let meaning = class_meaning(class);
            assert!(harness.text().contains(meaning), "{class:?}");
            screens.insert(harness.text());
        }
        assert_eq!(screens.len(), CLASSES.len());
    }

    #[test]
    fn failures_the_list_has_a_heading_and_a_row_per_failure_newest_first() {
        let harness = Harness::from_app(app_with(vec![
            (1, task_failed(FailureClass::AgentFailure, "first")),
            (2, task_failed(FailureClass::GitConflict, "second")),
        ]));
        let rows = rows(&harness);
        assert!(rows[2].starts_with("  TASK ATT CLASS"), "{}", rows[2]);
        assert!(rows[2].contains(" SIG "));
        assert!(rows[2].contains("REPEATS"));
        assert!(rows[2].contains("STATE"));
        assert!(rows[2].contains("DETAIL"));
        assert!(
            rows[3].starts_with("> 2    -   git_conflict"),
            "{}",
            rows[3]
        );
        assert!(rows[3].contains("×1/3"));
        assert!(rows[3].contains("failed"));
        assert!(rows[3].trim_end().ends_with("second"));
        assert!(
            rows[4].starts_with("  1    -   agent_failure"),
            "{}",
            rows[4]
        );
    }

    #[test]
    fn failures_a_tripped_breaker_is_a_full_width_white_on_red_bar_that_names_the_task() {
        let class = FailureClass::VerificationFailure;
        let harness = Harness::from_app(app_with(repeated(3, class, 3)));
        assert_eq!(
            rows(&harness)[1],
            format!(
                " !! CIRCUIT BREAKER TRIPPED · task 3 · {} ×3/3 · no more automatic retries",
                short(class)
            )
        );
        for x in 0..80 {
            let style = harness.buffer()[(x, 1)].style();
            assert_eq!(style.bg, Some(Color::Red), "column {x}");
            assert_eq!(style.fg, Some(Color::White), "column {x}");
            assert!(style.add_modifier.contains(Modifier::BOLD), "column {x}");
        }
    }

    #[test]
    fn failures_the_row_that_tripped_the_breaker_says_so_in_red() {
        let class = FailureClass::VerificationFailure;
        let harness = Harness::from_app(app_with(repeated(3, class, 3)));
        let rows = rows(&harness);
        assert!(rows[3].contains("×3/3 TRIPPED"), "{}", rows[3]);
        assert!(rows[4].contains("×2/3"), "{}", rows[4]);
        assert!(!rows[4].contains("TRIPPED"), "{}", rows[4]);
        let x = u16::try_from(rows[3].find("×3/3").expect("cell")).expect("narrow");
        assert_eq!(harness.buffer()[(x, 3)].style().bg, Some(Color::Red));
        assert_ne!(harness.buffer()[(x, 4)].style().bg, Some(Color::Red));
    }

    #[test]
    fn failures_without_a_trip_nothing_on_the_screen_is_red_backed_or_says_tripped() {
        let harness =
            Harness::from_app(app_with(repeated(3, FailureClass::VerificationFailure, 2)));
        assert!(!harness.text().contains("TRIPPED"));
        assert!(!harness.text().contains("CIRCUIT"));
        let area = harness.buffer().area;
        for y in 0..area.height {
            for x in 0..area.width {
                assert_ne!(
                    harness.buffer()[(x, y)].style().bg,
                    Some(Color::Red),
                    "{x},{y}"
                );
            }
        }
        assert_eq!(
            rows(&harness)[1].trim_end(),
            " circuit breaker: closed · most repeated 2× (trips at 3)"
        );
    }

    #[test]
    fn failures_the_banner_counts_the_other_tripped_tasks() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        events.extend(repeated(5, class, 4));
        let harness = Harness::from_app(app_with(events));
        let banner = rows(&harness)[1].clone();
        assert!(
            banner.starts_with(" !! CIRCUIT BREAKER TRIPPED · task 5 (+1 more) · "),
            "{banner}"
        );
        assert!(
            banner.contains(&format!("{} ×4/3", short(class))),
            "{banner}"
        );
    }

    #[test]
    fn failures_a_cancelled_task_no_longer_holds_the_banner_but_its_row_remembers() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        events.push((
            3,
            EventKind::TaskCancelled {
                reason: "no".into(),
            },
        ));
        let harness = Harness::from_app(app_with(events));
        let rows = rows(&harness);
        assert_eq!(rows[1].trim_end(), " circuit breaker: closed · no repeats");
        assert!(rows[3].contains("×3/3 TRIPPED"));
        assert!(rows[3].contains("cancelled"));
    }

    #[test]
    fn failures_snapshot_of_a_tripped_breaker_at_80x24() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        events.push((3, task_failed(class, "gave up")));
        events.push((
            2,
            EventKind::PreflightFailed {
                class: FailureClass::ProviderConfiguration,
                detail: "no API key".into(),
            },
        ));
        let mut harness = Harness::from_app(app_with(events));
        let config_sig = signature(FailureClass::ProviderConfiguration, &[]);
        let tripped_sig = signature(class, &[]);
        let expected = [
            "4 Failures".to_owned(),
            format!(
                " !! CIRCUIT BREAKER TRIPPED · task 3 · {} ×3/3 · no more automatic retries",
                &tripped_sig[..8]
            ),
            "  TASK ATT CLASS                  SIG      REPEATS      STATE       DETAIL".to_owned(),
            format!(
                "> 2    -   provider_configuration {} ×1/3         failed      no API key",
                &config_sig[..8]
            ),
            format!(
                "  3    3   verification_failure   {} ×3/3 TRIPPED failed      tests failed",
                &tripped_sig[..8]
            ),
            format!(
                "  3    2   verification_failure   {} ×2/3         failed      tests failed",
                &tripped_sig[..8]
            ),
            format!(
                "  3    1   verification_failure   {} ×1/3         failed      tests failed",
                &tripped_sig[..8]
            ),
        ];
        let shown = rows(&harness);
        for (at, want) in expected.iter().enumerate() {
            assert_eq!(shown[at].trim_end(), *want, "row {at}");
        }
        assert!(shown[7..14].iter().all(|row| row.trim().is_empty()));
        let detail = [
            "─".repeat(80),
            "provider_configuration — authentication, invalid model or missing executable"
                .to_owned(),
            "task 2 · attempt - · recorded by preflight · failed".to_owned(),
            format!("signature {config_sig} · seen 1× of 3 · breaker closed"),
            "detail: no API key".to_owned(),
            "policy: never loops: it pauses for a human at once".to_owned(),
            "actions: retry (r) · cancel (c)".to_owned(),
        ];
        for (at, want) in detail.iter().enumerate() {
            assert_eq!(shown[14 + at].trim_end(), *want, "row {}", 14 + at);
        }
        assert_eq!(shown[22].trim_end(), "r retry  c cancel  x rerun-gate");
        assert_eq!(shown[23].trim_end(), "Press ? for the key map");
        press(&mut harness, KeyCode::Char('j'));
        let shown = rows(&harness);
        assert!(shown[4].starts_with("> 3    3"), "{}", shown[4]);
        assert!(shown[17].contains("breaker TRIPPED"), "{}", shown[17]);
    }

    fn detail_row(harness: &Harness, prefix: &str) -> String {
        row_starting(harness, prefix)
            .unwrap_or_else(|| panic!("no {prefix} row in\n{}", harness.text()))
            .trim_end()
            .to_owned()
    }

    #[test]
    fn failures_each_class_of_failed_task_permits_its_own_actions() {
        use FailureClass::{
            AgentFailure, EnvironmentFailure, GitConflict, NeedsInput, PolicyFailure,
            ProviderConfiguration, ProviderLimit, ProviderTransient, VerificationFailure,
        };
        let retry_cancel = "actions: retry (r) · cancel (c)";
        let with_gates = "actions: retry (r) · cancel (c) · rerun-gate (x)";
        let cases = [
            (AgentFailure, retry_cancel),
            (VerificationFailure, with_gates),
            (ProviderLimit, retry_cancel),
            (ProviderTransient, retry_cancel),
            (ProviderConfiguration, retry_cancel),
            (GitConflict, retry_cancel),
            (EnvironmentFailure, with_gates),
            (PolicyFailure, retry_cancel),
            (NeedsInput, "actions: cancel (c) · resolve (input inbox, 6)"),
        ];
        for (class, expected) in cases {
            let harness = Harness::from_app(app_with(vec![(3, task_failed(class, "boom"))]));
            assert_eq!(detail_row(&harness, "actions:"), expected, "{class:?}");
        }
    }

    #[test]
    fn failures_a_task_still_being_remediated_can_only_be_cancelled() {
        let class = FailureClass::VerificationFailure;
        let harness = Harness::from_app(app_with(vec![(3, verify_failed(1, class, "x"))]));
        assert_eq!(detail_row(&harness, "actions:"), "actions: cancel (c)");
    }

    #[test]
    fn failures_a_finished_task_permits_nothing_and_says_why() {
        let class = FailureClass::VerificationFailure;
        for (end, why) in [
            (
                EventKind::TaskCancelled {
                    reason: "no".into(),
                },
                "cancelled",
            ),
            (
                EventKind::TaskDone {
                    commit: "abc".into(),
                },
                "recovered",
            ),
        ] {
            let harness = Harness::from_app(app_with(vec![(3, task_failed(class, "x")), (3, end)]));
            assert_eq!(
                detail_row(&harness, "actions:"),
                format!("actions: none · task 3 is {why}")
            );
        }
    }

    #[test]
    fn failures_the_detail_pane_reads_out_the_selected_failure() {
        let class = FailureClass::VerificationFailure;
        let sig = signature(class, &[]);
        let harness = Harness::from_app(app_with(repeated(3, class, 3)));
        assert_eq!(
            detail_row(&harness, "verification_failure —"),
            "verification_failure — tests, lint, build or privacy checks failed"
        );
        assert_eq!(
            detail_row(&harness, "task 3 ·"),
            "task 3 · attempt 3 · recorded by verification · remediating"
        );
        assert_eq!(
            detail_row(&harness, "signature"),
            format!("signature {sig} · seen 3× of 3 · breaker TRIPPED")
        );
        assert_eq!(detail_row(&harness, "detail:"), "detail: tests failed");
        assert!(detail_row(&harness, "policy:").contains("repeats trip the breaker"));
    }

    #[test]
    fn failures_the_detail_pane_shows_the_self_healing_report() {
        let class = FailureClass::VerificationFailure;
        let harness = Harness::from_app(app_with(vec![
            (3, verify_failed(1, class, "tests failed")),
            (
                3,
                EventKind::SelfHealingReport {
                    attempt: AttemptId::new(1),
                    class,
                    repairs: vec!["fixed the test".into(), "rebuilt".into()],
                    outcome: "passed on retry".into(),
                },
            ),
        ]));
        assert_eq!(
            detail_row(&harness, "healing:"),
            "healing: fixed the test; rebuilt → passed on retry"
        );
    }

    #[test]
    fn failures_the_action_bar_dims_what_the_selected_failure_does_not_permit() {
        let dim = |harness: &Harness, x: u16| {
            harness.buffer()[(x, 22)]
                .style()
                .add_modifier
                .contains(Modifier::DIM)
        };
        let agent = Harness::from_app(app_with(vec![(
            3,
            task_failed(FailureClass::AgentFailure, "x"),
        )]));
        assert_eq!(
            rows(&agent)[22].trim_end(),
            "r retry  c cancel  x rerun-gate"
        );
        assert!(!dim(&agent, 0), "retry is permitted");
        assert!(!dim(&agent, 10), "cancel is permitted");
        assert!(dim(&agent, 20), "an agent failure has no gates to re-run");
        let verification = Harness::from_app(app_with(vec![(
            3,
            task_failed(FailureClass::VerificationFailure, "x"),
        )]));
        assert!(!dim(&verification, 20));
        let needs_input = Harness::from_app(app_with(vec![(
            3,
            task_failed(FailureClass::NeedsInput, "x"),
        )]));
        assert!(dim(&needs_input, 0), "an answer, not a retry");
        assert!(!dim(&needs_input, 10));
    }

    fn one_failure(class: FailureClass) -> Harness {
        Harness::from_app(app_with(vec![(3, task_failed(class, "boom"))]))
    }

    #[test]
    fn failures_r_c_and_x_ask_for_the_action_on_the_selected_failures_task() {
        let mut harness = one_failure(FailureClass::VerificationFailure);
        for key in ['r', 'c', 'x'] {
            harness.key(key);
        }
        let task = TaskId::new(3);
        assert_eq!(
            harness.app().outbox,
            [
                Action::Retry { task },
                Action::Cancel { task },
                Action::RerunGate { task, gate: None },
            ]
        );
        assert_eq!(harness.app().notice, None);
    }

    #[test]
    fn failures_a_key_asks_for_the_action_on_the_task_under_the_cursor() {
        let mut harness = Harness::from_app(app_with(vec![
            (1, task_failed(FailureClass::AgentFailure, "x")),
            (2, task_failed(FailureClass::AgentFailure, "y")),
        ]));
        harness.key('j');
        harness.key('c');
        assert_eq!(
            harness.app().outbox,
            [Action::Cancel {
                task: TaskId::new(1)
            }]
        );
    }

    /// The journal events, the key pressed and the notice it leaves.
    type Refusal = (Vec<(u32, EventKind)>, char, &'static str);

    #[test]
    fn failures_a_refused_operation_asks_for_nothing_and_says_why() {
        let end = |event| (3, event);
        let cases: Vec<Refusal> = vec![
            (
                vec![(3, task_failed(FailureClass::NeedsInput, "x"))],
                'r',
                "retry: task 3 needs an answer, not another attempt; resolve it in the input inbox (6)",
            ),
            (
                vec![(3, task_failed(FailureClass::AgentFailure, "x"))],
                'x',
                "rerun-gate: task 3 has no gates to re-run after agent_failure",
            ),
            (
                vec![(3, verify_failed(1, FailureClass::VerificationFailure, "x"))],
                'r',
                "retry: task 3 is still being remediated",
            ),
            (
                vec![(3, verify_failed(1, FailureClass::VerificationFailure, "x"))],
                'x',
                "rerun-gate: task 3 is still being remediated",
            ),
            (
                vec![
                    (3, task_failed(FailureClass::AgentFailure, "x")),
                    end(EventKind::TaskCancelled {
                        reason: "no".into(),
                    }),
                ],
                'r',
                "retry: task 3 is cancelled",
            ),
            (
                vec![
                    (3, task_failed(FailureClass::AgentFailure, "x")),
                    end(EventKind::TaskDone {
                        commit: "abc".into(),
                    }),
                ],
                'c',
                "cancel: task 3 has since completed",
            ),
            (Vec::new(), 'r', "retry: no failure is selected"),
            (Vec::new(), 'c', "cancel: no failure is selected"),
            (Vec::new(), 'x', "rerun-gate: no failure is selected"),
        ];
        for (events, key, notice) in cases {
            let mut harness = Harness::from_app(app_with(events));
            harness.key(key);
            assert!(harness.app().outbox.is_empty(), "{key}: {notice}");
            assert_eq!(harness.app().notice.as_deref(), Some(notice));
            assert_eq!(
                rows(&harness)[22].trim_end(),
                notice.chars().take(80).collect::<String>()
            );
        }
    }

    #[test]
    fn failures_the_notice_goes_at_the_next_key_press() {
        let mut harness = Harness::from_app(app_with(Vec::new()));
        harness.key('r');
        assert!(harness.app().notice.is_some());
        harness.key('j');
        assert_eq!(harness.app().notice, None);
        assert!(!harness.text().contains("no failure is selected"));
    }

    #[test]
    fn failures_keys_do_nothing_on_other_screens_under_an_overlay_or_with_modifiers() {
        let base = app_with(vec![(
            3,
            task_failed(FailureClass::VerificationFailure, "x"),
        )]);
        for screen in Screen::ALL {
            if screen == Screen::Failures {
                continue;
            }
            let mut harness = Harness::from_app(App {
                screen,
                ..base.clone()
            });
            for key in ['r', 'c', 'x'] {
                harness.key(key);
            }
            assert!(harness.app().outbox.is_empty(), "{screen:?}");
        }
        let mut harness = Harness::from_app(App {
            overlay: Some(Overlay::KeyMap),
            ..base.clone()
        });
        harness.key('r');
        assert!(harness.app().outbox.is_empty());
        let mut harness = Harness::from_app(base);
        for code in ['r', 'c', 'x'] {
            for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
                harness.send(AppEvent::Key(KeyEvent::new(KeyCode::Char(code), modifiers)));
            }
        }
        assert!(harness.app().outbox.is_empty());
    }

    #[test]
    fn failures_movement_walks_the_list_from_the_newest_and_stops_at_both_ends() {
        let mut harness = Harness::from_app(app_with(vec![
            (1, task_failed(FailureClass::AgentFailure, "a")),
            (2, task_failed(FailureClass::AgentFailure, "b")),
            (3, task_failed(FailureClass::AgentFailure, "c")),
        ]));
        assert_eq!(selected_task(harness.app()), Some(3));
        let mut seen = Vec::new();
        for code in [
            KeyCode::Char('j'),
            KeyCode::Down,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Up,
            KeyCode::Up,
        ] {
            press(&mut harness, code);
            seen.push(selected_task(harness.app()));
        }
        assert_eq!(seen, [Some(2), Some(1), Some(1), Some(2), Some(3), Some(3)]);
        press(&mut harness, KeyCode::Char('G'));
        assert_eq!(selected_task(harness.app()), Some(1));
        press(&mut harness, KeyCode::Char('g'));
        assert_eq!(selected_task(harness.app()), Some(3));
    }

    #[test]
    fn failures_the_cursor_stays_on_its_failure_as_newer_ones_arrive_until_it_is_back_on_the_newest()
     {
        let mut harness = Harness::from_app(app_with(vec![
            (1, task_failed(FailureClass::AgentFailure, "a")),
            (2, task_failed(FailureClass::AgentFailure, "b")),
        ]));
        let arrive = |harness: &Harness, task: u32| {
            update(
                harness.app().clone(),
                AppEvent::Core(event(task, task_failed(FailureClass::AgentFailure, "n"))),
            )
        };
        // On the newest, the cursor follows it.
        harness = Harness::from_app(arrive(&harness, 3));
        assert_eq!(selected_task(harness.app()), Some(3));
        // Moved away, it stays where it is.
        press(&mut harness, KeyCode::Char('j'));
        harness = Harness::from_app(arrive(&harness, 4));
        assert_eq!(selected_task(harness.app()), Some(2));
        // Back on the newest, it follows again.
        press(&mut harness, KeyCode::Char('g'));
        harness = Harness::from_app(arrive(&harness, 5));
        assert_eq!(selected_task(harness.app()), Some(5));
        // Moving up to the newest is the same as `g`.
        press(&mut harness, KeyCode::Char('j'));
        press(&mut harness, KeyCode::Char('k'));
        harness = Harness::from_app(arrive(&harness, 6));
        assert_eq!(selected_task(harness.app()), Some(6));
    }

    #[test]
    fn failures_the_list_scrolls_to_keep_the_selection_in_view() {
        let events: Vec<(u32, EventKind)> = (1..=40)
            .map(|task| (task, task_failed(FailureClass::AgentFailure, "x")))
            .collect();
        let mut harness = Harness::from_app(app_with(events));
        assert!(row_starting(&harness, "> 40 ").is_some());
        press(&mut harness, KeyCode::Char('G'));
        let selected = row_starting(&harness, "> ").expect("the selection is drawn");
        assert!(selected.starts_with("> 1 "), "{selected}");
        assert!(harness.text().contains("provider") || harness.text().contains("agent_failure"));
        for _ in 0..3 {
            press(&mut harness, KeyCode::Char('k'));
        }
        let selected = row_starting(&harness, "> ").expect("the selection is drawn");
        assert!(selected.starts_with("> 4 "), "{selected}");
    }

    #[test]
    fn failures_small_terminals_keep_the_banner_and_never_panic() {
        let class = FailureClass::VerificationFailure;
        let mut events = repeated(3, class, 3);
        events.push((
            4,
            task_failed(FailureClass::NeedsInput, "漢字 ".repeat(100).as_str()),
        ));
        for size in [
            (0, 0),
            (1, 1),
            (2, 2),
            (20, 3),
            (20, 5),
            (40, 10),
            (79, 23),
            (80, 24),
            (200, 60),
        ] {
            let mut harness = Harness::from_app(app_at(size, events.clone()));
            for key in ['j', 'r', 'c', 'x', 'k', 'G'] {
                harness.key(key);
            }
            let rows = rows(&harness);
            if size.0 >= 20 && size.1 >= 2 {
                assert!(rows[1].starts_with(" !! CIRCUIT"), "{size:?}: {}", rows[1]);
            }
            if size.0 > 0 && size.1 > 0 {
                assert_eq!(rows.len(), usize::from(size.1), "{size:?}");
            }
        }
    }

    #[test]
    fn failures_a_reduced_terminal_still_lists_failures_below_the_banner() {
        let harness =
            Harness::from_app(app_at((20, 5), repeated(3, FailureClass::AgentFailure, 2)));
        let rows = rows(&harness);
        assert!(rows[2].starts_with("  TASK ATT CLASS"), "{}", rows[2]);
        assert!(rows[3].starts_with("> 3    2"), "{}", rows[3]);
        assert!(rows[4].starts_with("  3    1"), "{}", rows[4]);
    }

    #[test]
    fn failures_wide_text_and_a_long_detail_stay_inside_the_pane() {
        let wide = "漢字".repeat(200);
        let harness = Harness::from_app(app_with(vec![(
            3,
            task_failed(FailureClass::GitConflict, &wide),
        )]));
        let rows = rows(&harness);
        assert_eq!(rows[0].trim_end(), "4 Failures");
        assert_eq!(rows[23].trim_end(), "Press ? for the key map");
        let with_text: Vec<&String> = rows.iter().filter(|row| row.contains('漢')).collect();
        assert_eq!(
            with_text.len(),
            2,
            "the list row and the detail line: {rows:?}"
        );
        for row in with_text {
            assert!(row.matches('漢').count() * 2 <= 80, "{row}");
        }
    }
}

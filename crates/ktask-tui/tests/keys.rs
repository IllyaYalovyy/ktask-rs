//! Every key binding docs/CONTRACT.md §4 documents, sent to the interface on
//! each screen it applies to, with the effect it is documented to have
//! asserted (docs/CONTRACT.md §5: "every key binding ... is exercised by a
//! scripted event sequence").
//!
//! Three things are compared and none is derived from another:
//!
//! - the **documentation**: the key blocks of §4 are read from
//!   `docs/CONTRACT.md` itself, so a binding added to the contract is a key
//!   this file has to know about;
//! - the **table**: [`BINDINGS`], which the key map overlay is drawn from and
//!   which resolves the keys the interface shares between screens;
//! - the **cases** below: one per key and screen, each building a state where
//!   the key has something to do, pressing it through the real [`update`] and
//!   asserting what came of it: the selection that moved, the action that
//!   reached the outbox, the screen that opened.
//!
//! [`keys_every_documented_binding_has_a_case`] fails for a key the contract
//! documents that no case covers, [`keys_every_case_is_for_a_documented_binding`]
//! for a case whose key the contract no longer documents, and
//! [`keys_every_case_agrees_with_the_table`] for a key whose row in the table
//! is not the one the case expects, or that is in the table and documented
//! nowhere. [`keys_every_case_passes`] then runs them all and reports every
//! failure, not the first.
//!
//! Screen keys such as the queue's `p` are not rows of the table: the screen
//! that owns them reads them itself. A case says which by naming the
//! [`KeyAction`] the table gives the key on that screen, or `None`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{
    AttemptId, CheckResult, CheckStatus, DecisionRequest, Event, EventKind, EventSeq, FailureClass,
    GateKind, GateResult, Journal, Level, PauseReason, Phase, Setting, Source, Stream, TaskId,
};
use ktask_tui::screen::config::{self, Snapshot};
use ktask_tui::screen::git::{self, Repo};
use ktask_tui::screen::{history, queue};
use ktask_tui::terminal::quits;
use ktask_tui::testing::Harness;
use ktask_tui::{
    Action, App, AppEvent, BINDINGS, KeyAction, Overlay, Screen, TaskView, lookup, update,
};
use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const SIZE: (u16, u16) = (80, 24);

/// What a helper that can fail returns, so that a failure reaches the case
/// that asked for the state.
type Fallible<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// A key as the table and the contract name it: the code and the modifiers,
/// with Shift folded into the key where it is part of it (`G`, Shift-Tab).
type Key = (KeyCode, KeyModifiers);

// ---- keys ----

/// The key called `name` in the contract's key blocks (`j`, `Tab`, `S-Tab`,
/// `PgUp`, `Ctrl-C`, `F1`), or an error naming what was not understood.
fn key(name: &str) -> Fallible<Key> {
    let none = KeyModifiers::NONE;
    if let Some(letter) = name.strip_prefix("Ctrl-") {
        let mut chars = letter.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) => Ok((KeyCode::Char(c.to_ascii_lowercase()), KeyModifiers::CONTROL)),
            _ => Err(format!("unknown key {name:?}").into()),
        };
    }
    let code = match name {
        "Tab" => KeyCode::Tab,
        "S-Tab" | "Shift-Tab" => KeyCode::BackTab,
        "Esc" => KeyCode::Esc,
        "Enter" => KeyCode::Enter,
        "Up" | "up" => KeyCode::Up,
        "Down" | "dn" => KeyCode::Down,
        "PgUp" => KeyCode::PageUp,
        "PgDn" => KeyCode::PageDown,
        "F1" => KeyCode::F(1),
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return Err(format!("unknown key {name:?}").into()),
            }
        }
    };
    Ok((code, none))
}

/// `key`, as a terminal reports it: an upper-case letter with Shift held.
fn event_of((code, modifiers): Key) -> KeyEvent {
    match code {
        KeyCode::Char(c) if c.is_uppercase() => {
            KeyEvent::new(code, modifiers | KeyModifiers::SHIFT)
        }
        _ => KeyEvent::new(code, modifiers),
    }
}

/// Every way a terminal reports `key`: with and without the Shift that made
/// it, since a terminal may or may not say so.
fn reports((code, modifiers): Key) -> Vec<KeyEvent> {
    let plain = KeyEvent::new(code, modifiers);
    let shifted = KeyEvent::new(code, modifiers | KeyModifiers::SHIFT);
    match code {
        KeyCode::Char(c) if c.is_uppercase() || c == '?' => vec![shifted, plain],
        KeyCode::BackTab => vec![
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            plain,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ],
        _ => vec![plain],
    }
}

/// `key` in the form the table compares: Shift folded into the key.
fn canonical(event: &KeyEvent) -> Key {
    match event.code {
        KeyCode::Tab if event.modifiers.contains(KeyModifiers::SHIFT) => {
            (KeyCode::BackTab, event.modifiers - KeyModifiers::SHIFT)
        }
        code => (code, event.modifiers - KeyModifiers::SHIFT),
    }
}

// ---- what the contract documents ----

/// A key the contract documents on a screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Documented {
    screen: Screen,
    key: Key,
    /// The line of the contract it was read from.
    line: String,
}

/// The keys a line of a key block starts with, and the description after
/// them. The key column is one key or several joined by `/`, `or` or a comma
/// (`j/k, up/dn`, `Tab / S-Tab`, `? or F1`), or the range `1..9`; the
/// description follows without a joiner. A column may be so wide that only one
/// space separates the two.
fn keys_of(line: &str) -> Fallible<(Vec<Key>, String)> {
    let mut words = line.split_whitespace().peekable();
    let mut keys = Vec::new();
    while let Some(raw) = words.next() {
        let name = raw.trim_end_matches(',');
        match name {
            "1..9" => {
                for digit in '1'..='9' {
                    keys.push(key(&digit.to_string())?);
                }
            }
            joined if joined.len() > 1 && joined.contains('/') => {
                for each in joined.split('/') {
                    keys.push(key(each)?);
                }
            }
            single => keys.push(key(single)?),
        }
        if raw.ends_with(',') || matches!(words.peek(), Some(&("/" | "or"))) {
            if matches!(words.peek(), Some(&("/" | "or"))) {
                words.next();
            }
        } else {
            break;
        }
    }
    Ok((keys, words.collect::<Vec<_>>().join(" ")))
}

/// The screen a numbered list item of §4 introduces (`3. **Logs** — ...`).
fn screen_of_item(line: &str) -> Option<Screen> {
    let (number, rest) = line.split_once(". **")?;
    let number: usize = number.trim().parse().ok()?;
    let _ = rest;
    Screen::ALL.get(number.checked_sub(1)?).copied()
}

/// Keys the contract documents in a sentence rather than in a key block, on
/// the screen, with the words that document each: the live run has no block.
const PROSE: [(Screen, &str, &str); 1] = [(Screen::LiveRun, "f", "`f` re-attaches")];

/// Every key block of section 4 of `contract`: the global keys apply on every
/// screen, and each screen's own follow its list item.
fn documented_in(contract: &str) -> Fallible<Vec<Documented>> {
    let start = contract
        .find("## 4. The TUI")
        .ok_or("docs/CONTRACT.md has no section 4")?;
    let section = contract.get(start..).ok_or("section 4 is unreadable")?;
    let end = section.find("\n## 5.").ok_or("section 4 has no end")?;
    let section = section.get(..end).ok_or("section 4 is unreadable")?;

    let mut found: Vec<Documented> = Vec::new();
    let mut applies: Vec<Screen> = Vec::new();
    let mut in_block = false;
    for line in section.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_block = !in_block;
        } else if in_block {
            if trimmed.is_empty() {
                continue;
            }
            for key in keys_of(trimmed)?.0 {
                for screen in &applies {
                    let entry = Documented {
                        screen: *screen,
                        key,
                        line: trimmed.to_owned(),
                    };
                    if !found
                        .iter()
                        .any(|seen| (seen.screen, seen.key) == (entry.screen, entry.key))
                    {
                        found.push(entry);
                    }
                }
            }
        } else if trimmed.starts_with("### Global keys") {
            applies = Screen::ALL.to_vec();
        } else if trimmed.starts_with("###") {
            applies.clear();
        } else if let Some(screen) = screen_of_item(trimmed) {
            applies = vec![screen];
        }
    }
    for (screen, name, words) in PROSE {
        if section.contains(words) {
            found.push(Documented {
                screen,
                key: key(name)?,
                line: words.to_owned(),
            });
        }
    }
    Ok(found)
}

fn documented() -> Fallible<Vec<Documented>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/CONTRACT.md");
    documented_in(&std::fs::read_to_string(path)?)
}

// ---- the interface, driven the way the shell drives it ----

/// An [`App`] fed keys through the real [`update`] and refilled after every
/// one the way the shell refills the screens that read the journal, the
/// repository and the configuration.
struct Session {
    app: App,
    refill: Box<dyn Fn(&mut App) -> Fallible>,
    /// What the refill reads, kept alive as long as the session.
    _keep: Vec<Box<dyn Any>>,
}

impl Session {
    /// A session over a state that needs no reading.
    fn new(app: App) -> Self {
        Self {
            app,
            refill: Box::new(|_| Ok(())),
            _keep: Vec::new(),
        }
    }

    /// A session whose screen reads what it shows: `refill` runs now and
    /// after every key.
    fn reading(
        app: App,
        keep: Vec<Box<dyn Any>>,
        refill: impl Fn(&mut App) -> Fallible + 'static,
    ) -> Fallible<Self> {
        let mut session = Self {
            app,
            refill: Box::new(refill),
            _keep: keep,
        };
        (session.refill)(&mut session.app)?;
        Ok(session)
    }

    fn send(&mut self, key: &KeyEvent) -> Fallible {
        self.app = update(self.app.clone(), AppEvent::Key(*key));
        (self.refill)(&mut self.app)
    }

    /// Presses the key called `name`.
    fn press(&mut self, name: &str) -> Fallible {
        self.send(&event_of(key(name)?))
    }

    fn press_all(&mut self, names: &[&str]) -> Fallible {
        names.iter().try_for_each(|name| self.press(name))
    }

    /// Types `text` a letter at a time.
    fn type_text(&mut self, text: &str) -> Fallible {
        text.chars()
            .try_for_each(|c| self.send(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)))
    }

    /// The screen as an operator sees it, one line per row.
    fn text(&self) -> String {
        Harness::from_app(self.app.clone()).text()
    }
}

// ---- building states ----

fn on(screen: Screen) -> App {
    App {
        screen,
        ..App::new(SIZE)
    }
}

/// The instant of the `n`th fixture event.
fn at(n: u32) -> OffsetDateTime {
    datetime!(2026-09-23 10:30:00 UTC) + Duration::seconds(10 * i64::from(n))
}

fn event(n: u32, task: u32, kind: EventKind) -> Event {
    Event {
        seq: EventSeq::new(u64::from(n)),
        ts: at(n),
        task_id: Some(TaskId::new(task)),
        kind,
    }
}

/// Folds `kinds`, each for `task`, into `app` as journal events.
fn feed(app: App, task: u32, kinds: Vec<EventKind>) -> App {
    kinds.into_iter().zip(1..).fold(app, |app, (kind, n)| {
        update(app, AppEvent::Core(event(n, task, kind)))
    })
}

fn queued(title: &str) -> EventKind {
    EventKind::TaskQueued {
        title: title.to_owned(),
    }
}

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn started(attempt: u32, protocol: &str) -> EventKind {
    EventKind::AttemptStarted {
        attempt: AttemptId::new(attempt),
        protocol: protocol.to_owned(),
        pid: 4242,
        base_sha: BASE.to_owned(),
    }
}

fn phase(phase: Phase) -> EventKind {
    EventKind::PhaseEntered {
        attempt: AttemptId::new(1),
        phase,
    }
}

fn output(text: &str) -> EventKind {
    EventKind::AgentOutput {
        attempt: AttemptId::new(1),
        stream: Stream::Stdout,
        text: text.to_owned(),
    }
}

fn failed_gate(stderr: &str) -> EventKind {
    EventKind::GateFinished {
        result: GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 2_400,
            stdout: String::new(),
            stderr: stderr.to_owned(),
            timed_out: false,
        },
    }
}

/// Everything up to an agent working in the green phase of a `tdd` attempt.
fn running() -> Vec<EventKind> {
    vec![
        queued("Add the parser"),
        EventKind::PreflightStarted,
        EventKind::PreflightPassed {
            base_sha: BASE.to_owned(),
        },
        started(1, "tdd"),
        phase(Phase::Green),
    ]
}

/// A run whose verification failed and whose task then failed.
fn failed_run() -> Vec<EventKind> {
    let mut kinds = running();
    kinds.extend([
        failed_gate("error[E0308]: mismatched types"),
        EventKind::VerifyFailed {
            attempt: AttemptId::new(1),
            class: FailureClass::VerificationFailure,
            detail: "the verify gate failed with exit code 1".to_owned(),
        },
        EventKind::TaskFailed {
            class: FailureClass::VerificationFailure,
            detail: "the verify gate failed with exit code 1".to_owned(),
        },
    ]);
    kinds
}

/// A task waiting for the operator's answer, with a recommended response.
fn asking() -> Vec<EventKind> {
    let mut kinds = running();
    kinds.extend([
        EventKind::DecisionRaised {
            request: DecisionRequest {
                question: "Should the parser reject trailing commas?".to_owned(),
                options: vec!["Reject them".to_owned(), "Accept them".to_owned()],
                tradeoffs: "Rejecting is stricter".to_owned(),
                impact: "Every task file already written".to_owned(),
                recommended: Some("Reject them".to_owned()),
            },
        },
        EventKind::Paused {
            reason: PauseReason::Input,
        },
    ]);
    kinds
}

/// A row of the queue as the shell builds it.
fn row(id: u32, state: &str) -> TaskView {
    TaskView {
        id: TaskId::new(id),
        title: format!("Task number {id}"),
        state: state.to_owned(),
        protocol: String::new(),
        phase: None,
        attempts: 0,
        elapsed: None,
    }
}

/// The queue holding one task per state, in order.
fn queue_of(states: &[&str]) -> App {
    App {
        tasks: (1..)
            .zip(states)
            .map(|(id, state)| row(id, state))
            .collect(),
        ..on(Screen::Queue)
    }
}

/// `screen` with `count` tasks queued.
fn tasks_on(screen: Screen, count: u32) -> App {
    (1..=count).fold(on(screen), |app, id| {
        feed(app, id, vec![queued(&format!("Task number {id}"))])
    })
}

/// The numbers that follow `prefix` in `text`, in the order they appear.
fn numbers(text: &str, prefix: &str) -> Vec<u32> {
    text.match_indices(prefix)
        .filter_map(|(at, _)| {
            let rest = text.get(at + prefix.len()..)?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect()
}

// ---- cases ----

/// One key on one screen and what it is documented to do there.
struct Case {
    screen: Screen,
    key: Key,
    /// What the table resolves the key to on this screen: `None` for a key
    /// the screen reads itself.
    via: Option<KeyAction>,
    what: &'static str,
    /// Builds a state where the key has something to do, sends it the event
    /// and asserts the outcome.
    run: Box<dyn Fn(&KeyEvent) -> Fallible>,
}

fn case(
    screen: Screen,
    name: &str,
    via: Option<KeyAction>,
    what: &'static str,
    run: impl Fn(&KeyEvent) -> Fallible + 'static,
) -> Fallible<Case> {
    Ok(Case {
        screen,
        key: key(name)?,
        via,
        what,
        run: Box::new(run),
    })
}

/// A case that presses the key on `build`'s state and asserts on the session.
fn on_session(
    screen: Screen,
    name: &str,
    via: Option<KeyAction>,
    what: &'static str,
    build: fn() -> Fallible<Session>,
    check: impl Fn(&Session) + 'static,
) -> Fallible<Case> {
    case(screen, name, via, what, move |event| {
        let mut session = build()?;
        session.send(event)?;
        check(&session);
        Ok(())
    })
}

/// A case that presses the key on the state `build` makes and asserts the
/// actions that reached the outbox, and no notice of a refusal.
fn dispatches(
    screen: Screen,
    name: &str,
    what: &'static str,
    build: fn() -> App,
    expected: Vec<Action>,
) -> Fallible<Case> {
    case(screen, name, None, what, move |event| {
        let mut session = Session::new(build());
        session.send(event)?;
        assert_eq!(session.app.outbox, expected);
        assert_eq!(session.app.notice, None);
        Ok(())
    })
}

/// The table's action for the movement keys, by name.
fn moves(name: &str) -> Option<KeyAction> {
    match name {
        "j" | "Down" => Some(KeyAction::MoveDown),
        "k" | "Up" => Some(KeyAction::MoveUp),
        "g" => Some(KeyAction::First),
        "G" => Some(KeyAction::Last),
        _ => None,
    }
}

const MOVEMENT: [&str; 6] = ["j", "Down", "k", "Up", "g", "G"];

fn task(id: u32) -> TaskId {
    TaskId::new(id)
}

// ---- keys on every screen ----

fn every_screen_cases(cases: &mut Vec<Case>) -> Fallible {
    for screen in Screen::ALL {
        for target in Screen::ALL {
            let digit = (target as u8).to_string();
            cases.push(case(
                screen,
                &digit,
                Some(KeyAction::Jump(target)),
                "shows the screen of that number",
                move |event| {
                    let mut session = Session::new(on(screen));
                    session.send(event)?;
                    assert_eq!(session.app.screen, target);
                    let header = session.text();
                    assert!(
                        header.starts_with(&format!("{} ", target as u8)),
                        "{header}"
                    );
                    Ok(())
                },
            )?);
        }
        let index = Screen::ALL.iter().position(|s| *s == screen).unwrap_or(0);
        let around = |by: usize| {
            Screen::ALL
                .into_iter()
                .cycle()
                .nth(index + by)
                .unwrap_or(screen)
        };
        let (next, previous) = (around(1), around(Screen::ALL.len() - 1));
        for (name, action, target) in [
            ("Tab", KeyAction::NextScreen, next),
            ("S-Tab", KeyAction::PrevScreen, previous),
        ] {
            cases.push(case(
                screen,
                name,
                Some(action),
                "shows the next or previous screen, wrapping",
                move |event| {
                    let mut session = Session::new(on(screen));
                    session.send(event)?;
                    assert_eq!(session.app.screen, target);
                    Ok(())
                },
            )?);
        }
        for name in ["?", "F1"] {
            cases.push(case(
                screen,
                name,
                Some(KeyAction::ShowKeyMap),
                "opens the key map overlay",
                move |event| {
                    let mut session = Session::new(on(screen));
                    session.send(event)?;
                    assert_eq!(session.app.overlay, Some(Overlay::KeyMap));
                    assert!(session.text().contains("Key map"));
                    Ok(())
                },
            )?);
        }
        cases.push(case(
            screen,
            "Esc",
            Some(KeyAction::Back),
            "dismisses the overlay",
            move |event| {
                let mut session = Session::new(on(screen));
                session.press("?")?;
                assert_eq!(session.app.overlay, Some(Overlay::KeyMap));
                session.send(event)?;
                assert_eq!(session.app.overlay, None);
                assert!(!session.text().contains("Key map"));
                Ok(())
            },
        )?);
        cases.push(case(
            screen,
            "q",
            Some(KeyAction::QuitOrClose),
            "quits, or closes the overlay when one is open",
            move |event| {
                let mut session = Session::new(on(screen));
                assert!(quits(&session.app, event));
                let before = session.app.clone();
                session.send(event)?;
                assert_eq!(
                    session.app, before,
                    "the shell quits; the state is not changed"
                );
                session.press("?")?;
                assert!(!quits(&session.app, event));
                session.send(event)?;
                assert_eq!(session.app.overlay, None);
                Ok(())
            },
        )?);
        cases.push(case(
            screen,
            "Ctrl-C",
            Some(KeyAction::Quit),
            "quits, and asks nothing of a running task",
            move |event| {
                let mut session = Session::new(App {
                    screen,
                    ..queue_of(&["Running", "Queued"])
                });
                assert!(quits(&session.app, event));
                let before = session.app.clone();
                session.send(event)?;
                assert_eq!(session.app, before);
                assert!(session.app.outbox.is_empty());
                session.press("?")?;
                assert!(quits(&session.app, event), "even with the key map open");
                Ok(())
            },
        )?);
    }
    Ok(())
}

/// The search key. Only the logs screen has a search to start; on the others
/// the table resolves the key and nothing more is done with it.
fn search_cases(cases: &mut Vec<Case>) -> Fallible {
    for screen in Screen::ALL {
        if screen == Screen::Logs {
            continue;
        }
        cases.push(case(
            screen,
            "/",
            Some(KeyAction::Search),
            "resolves to search; this screen has nothing to search yet",
            move |event| {
                let found = lookup(screen, event).map(|binding| binding.action);
                assert_eq!(found, Some(KeyAction::Search));
                Ok(())
            },
        )?);
    }
    Ok(())
}

// ---- movement ----

/// A screen with rows to select among, and how to read the selection.
#[derive(Clone, Copy)]
struct Rows {
    screen: Screen,
    build: fn() -> Fallible<Session>,
    /// The keys that put the selection on the middle one of five rows.
    to_middle: &'static [&'static str],
    /// The selected row, counting from the one `g` selects on this screen.
    at: fn(&App) -> usize,
}

const FIVE: usize = 5;

fn git_rows() -> Fallible<Session> {
    git_session()
}

fn rows_of_screens() -> [Rows; 6] {
    [
        Rows {
            screen: Screen::Queue,
            build: || Ok(Session::new(tasks_on(Screen::Queue, 5))),
            to_middle: &["j", "j"],
            at: |app| queue::selected_row(app).unwrap_or(0),
        },
        Rows {
            screen: Screen::Logs,
            build: || Ok(Session::new(tasks_on(Screen::Logs, 5))),
            to_middle: &["k", "k"],
            at: |app| {
                app.logs
                    .selected()
                    .map_or(0, |entry| usize::try_from(entry.number()).unwrap_or(0))
            },
        },
        Rows {
            screen: Screen::Failures,
            build: || {
                let app = (1..=5).fold(on(Screen::Failures), |app, id| {
                    let kinds = vec![
                        queued("A task"),
                        EventKind::PreflightStarted,
                        EventKind::PreflightFailed {
                            class: FailureClass::EnvironmentFailure,
                            detail: format!("failure {id}"),
                        },
                    ];
                    feed(app, id, kinds)
                });
                Ok(Session::new(app))
            },
            to_middle: &["j", "j"],
            at: |app| {
                let number = app
                    .failures
                    .selected()
                    .map(ktask_tui::screen::failures::Failure::number);
                app.failures
                    .newest_first()
                    .position(|failure| Some(failure.number()) == number)
                    .unwrap_or(0)
            },
        },
        Rows {
            screen: Screen::Inspector,
            build: || Ok(Session::new(tasks_on(Screen::Inspector, 5))),
            to_middle: &["j", "j"],
            at: |app| app.selected.get(&Screen::Inspector).copied().unwrap_or(0),
        },
        Rows {
            screen: Screen::InputInbox,
            build: || {
                let app = (1..=5).fold(on(Screen::InputInbox), |app, id| feed(app, id, asking()));
                Ok(Session::new(app))
            },
            to_middle: &["j", "j"],
            at: |app| {
                let selected = app.inbox.selected();
                app.inbox
                    .tasks()
                    .position(|task| Some(task) == selected)
                    .unwrap_or(0)
            },
        },
        Rows {
            screen: Screen::Git,
            build: git_rows,
            to_middle: &["j", "j"],
            at: |app| app.git.selected_file(),
        },
    ]
}

/// `j`, `k`, the arrows, `g` and `G` on the screens that select among rows:
/// from the middle of five they go one down, one up, to the first and to the
/// last.
fn movement_cases(cases: &mut Vec<Case>) -> Fallible {
    for rows in rows_of_screens() {
        for name in MOVEMENT {
            let expected = match name {
                "j" | "Down" => 3,
                "k" | "Up" => 1,
                "g" => 0,
                _ => FIVE - 1,
            };
            cases.push(case(
                rows.screen,
                name,
                moves(name),
                "moves the selection",
                move |event| {
                    let mut session = (rows.build)()?;
                    session.press_all(rows.to_middle)?;
                    assert_eq!(
                        (rows.at)(&session.app),
                        2,
                        "the setup did not reach the middle"
                    );
                    session.send(event)?;
                    assert_eq!((rows.at)(&session.app), expected);
                    Ok(())
                },
            )?);
        }
    }
    Ok(())
}

// ---- queue ----

fn queue_actions(cases: &mut Vec<Case>) -> Fallible {
    let in_flight = || queue_of(&["Done", "Running"]);
    cases.push(dispatches(
        Screen::Queue,
        "p",
        "asks for the run to pause",
        in_flight,
        vec![Action::Pause],
    )?);
    cases.push(dispatches(
        Screen::Queue,
        "i",
        "asks for the run to be interrupted",
        in_flight,
        vec![Action::Interrupt],
    )?);
    cases.push(dispatches(
        Screen::Queue,
        "R",
        "asks for the queue to resume",
        || queue_of(&["Done", "Queued"]),
        vec![Action::Resume],
    )?);
    cases.push(dispatches(
        Screen::Queue,
        "r",
        "asks for the selected failed task to be retried",
        || {
            let mut app = queue_of(&["Done", "Failed", "Queued"]);
            app.selected.insert(Screen::Queue, 1);
            app
        },
        vec![Action::Retry { task: task(2) }],
    )?);
    cases.push(dispatches(
        Screen::Queue,
        "c",
        "asks for the selected task to be cancelled",
        || {
            let mut app = queue_of(&["Done", "Queued", "Queued"]);
            app.selected.insert(Screen::Queue, 1);
            app
        },
        vec![Action::Cancel { task: task(2) }],
    )?);
    cases.push(dispatches(
        Screen::Queue,
        "x",
        "asks for the selected task's gates to be run again",
        || {
            let mut app = queue_of(&["Done", "Failed"]);
            app.selected.insert(Screen::Queue, 1);
            app
        },
        vec![Action::RerunGate {
            task: task(2),
            gate: None,
        }],
    )?);
    cases.push(dispatches(
        Screen::Queue,
        "A",
        "asks for the human gate to be acknowledged",
        || {
            let mut kinds = running();
            kinds.push(EventKind::Paused {
                reason: PauseReason::HumanGate,
            });
            feed(on(Screen::Queue), 1, kinds)
        },
        vec![Action::Acknowledge {
            task: Some(task(1)),
        }],
    )?);
    Ok(())
}

/// The queue's view operations: they change what is shown and ask nothing of
/// the supervisor.
fn queue_views(cases: &mut Vec<Case>) -> Fallible {
    cases.push(case(
        Screen::Queue,
        "a",
        None,
        "attaches: shows the live run, following it",
        |event| {
            let mut app = queue_of(&["Done", "Running"]);
            app.follow = false;
            app.scroll.insert(Screen::LiveRun, 7);
            let mut session = Session::new(app);
            session.send(event)?;
            assert_eq!(session.app.screen, Screen::LiveRun);
            assert!(session.app.follow);
            assert_eq!(session.app.scroll.get(&Screen::LiveRun), None);
            assert!(session.app.outbox.is_empty());
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::Queue,
        "d",
        None,
        "opens the selected task's diff on the git screen",
        |event| {
            let mut app = queue_of(&["Done", "Failed", "Queued"]);
            app.selected.insert(Screen::Queue, 2);
            let mut session = Session::new(app);
            session.send(event)?;
            assert_eq!(session.app.screen, Screen::Git);
            assert_eq!(session.app.selected.get(&Screen::Queue), Some(&2));
            assert!(session.app.outbox.is_empty());
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::Queue,
        "Enter",
        None,
        "opens the selected task in the inspector",
        |event| {
            let mut app = queue_of(&["Done", "Failed", "Queued"]);
            app.selected.insert(Screen::Queue, 2);
            let mut session = Session::new(app);
            session.send(event)?;
            assert_eq!(session.app.screen, Screen::Inspector);
            assert_eq!(session.app.selected.get(&Screen::Inspector), Some(&2));
            assert!(session.app.outbox.is_empty());
            Ok(())
        },
    )?);
    Ok(())
}

// ---- live run ----

/// The live run screen holding a hundred lines of output, `line 1` to
/// `line 100`, following.
fn live_app() -> App {
    let mut kinds = running();
    kinds.extend((1..=100).map(|n| output(&format!("line {n}\n"))));
    feed(on(Screen::LiveRun), 1, kinds)
}

/// The live run scrolled five lines up from the newest and not following.
fn live_scrolled() -> Session {
    let mut app = live_app();
    app.follow = false;
    app.scroll.insert(Screen::LiveRun, 5);
    Session::new(app)
}

fn live_cases(cases: &mut Vec<Case>) -> Fallible {
    for name in ["k", "Up"] {
        cases.push(case(
            Screen::LiveRun,
            name,
            moves(name),
            "scrolls up a line and stops following",
            |event| {
                let mut session = Session::new(live_app());
                assert_eq!(numbers(&session.text(), "line ").last(), Some(&100));
                session.send(event)?;
                assert_eq!(numbers(&session.text(), "line ").last(), Some(&99));
                assert!(!session.app.follow);
                Ok(())
            },
        )?);
    }
    for name in ["j", "Down"] {
        cases.push(case(
            Screen::LiveRun,
            name,
            moves(name),
            "scrolls down a line without following again",
            |event| {
                let mut session = live_scrolled();
                assert_eq!(numbers(&session.text(), "line ").last(), Some(&95));
                session.send(event)?;
                assert_eq!(numbers(&session.text(), "line ").last(), Some(&96));
                assert!(!session.app.follow);
                Ok(())
            },
        )?);
    }
    cases.push(case(
        Screen::LiveRun,
        "g",
        moves("g"),
        "scrolls to the oldest line held and stops following",
        |event| {
            let mut session = Session::new(live_app());
            session.send(event)?;
            assert_eq!(numbers(&session.text(), "line ").first(), Some(&1));
            assert!(!session.app.follow);
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::LiveRun,
        "G",
        moves("G"),
        "scrolls to the newest line without following again",
        |event| {
            let mut session = live_scrolled();
            session.send(event)?;
            assert_eq!(numbers(&session.text(), "line ").last(), Some(&100));
            assert!(!session.app.follow);
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::LiveRun,
        "f",
        Some(KeyAction::Follow),
        "follows the newest output again",
        |event| {
            let mut session = live_scrolled();
            session.send(event)?;
            assert!(session.app.follow);
            assert_eq!(numbers(&session.text(), "line ").last(), Some(&100));
            assert!(session.text().contains("following"));
            Ok(())
        },
    )?);
    Ok(())
}

// ---- logs ----

/// The logs after `titles` were queued, one task each: an entry apiece.
fn logs_of(titles: &[&str]) -> App {
    (1..)
        .zip(titles)
        .fold(on(Screen::Logs), |app, (id, title)| {
            feed(app, id, vec![queued(title)])
        })
}

fn logs_search_cases(cases: &mut Vec<Case>) -> Fallible {
    cases.push(case(
        Screen::Logs,
        "/",
        Some(KeyAction::Search),
        "types a search: Enter keeps it and Esc clears it later",
        |event| {
            let mut session = Session::new(logs_of(&["one", "two"]));
            session.send(event)?;
            assert!(session.app.logs.is_typing());
            session.type_text("tw")?;
            assert_eq!(session.app.search, None, "not kept until Enter");
            session.press("Enter")?;
            assert!(!session.app.logs.is_typing());
            assert_eq!(session.app.search.as_deref(), Some("tw"));
            session.press("Esc")?;
            assert_eq!(session.app.search, None);
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::Logs,
        "Esc",
        Some(KeyAction::Back),
        "clears the search, or abandons one being typed",
        |event| {
            let mut app = logs_of(&["one", "two"]);
            app.search = Some("one".to_owned());
            let mut session = Session::new(app);
            session.send(event)?;
            assert_eq!(session.app.search, None);

            let mut app = logs_of(&["one", "two"]);
            app.search = Some("one".to_owned());
            let mut session = Session::new(app);
            session.press("/")?;
            session.type_text("tw")?;
            session.send(event)?;
            assert!(!session.app.logs.is_typing());
            assert_eq!(
                session.app.search.as_deref(),
                Some("one"),
                "abandoned, not kept"
            );
            Ok(())
        },
    )?);
    let titles = ["match one", "other", "match two", "other two"];
    for (name, via, expected) in [
        ("n", KeyAction::NextMatch, 0),
        ("N", KeyAction::PrevMatch, 2),
    ] {
        cases.push(case(
            Screen::Logs,
            name,
            Some(via),
            "goes to the next or previous match, wrapping",
            move |event| {
                let mut app = logs_of(&titles);
                app.search = Some("match".to_owned());
                let mut session = Session::new(app);
                assert_eq!(selected_entry(&session.app), 3);
                session.send(event)?;
                assert_eq!(selected_entry(&session.app), expected);
                Ok(())
            },
        )?);
    }
    Ok(())
}

/// The number of the entry the logs cursor is on.
fn selected_entry(app: &App) -> u64 {
    app.logs
        .selected()
        .map_or(u64::MAX, ktask_tui::screen::logs::Entry::number)
}

fn logs_cases(cases: &mut Vec<Case>) -> Fallible {
    cases.push(case(
        Screen::Logs,
        "v",
        Some(KeyAction::ToggleView),
        "switches between the structured and the raw view",
        |event| {
            let mut session = Session::new(logs_of(&["one"]));
            assert!(session.app.logs.is_structured());
            session.send(event)?;
            assert!(!session.app.logs.is_structured());
            session.send(event)?;
            assert!(session.app.logs.is_structured());
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::Logs,
        "l",
        Some(KeyAction::CycleLevel),
        "raises the minimum level, wrapping after error",
        |event| {
            let mut session = Session::new(logs_of(&["one"]));
            let mut seen = vec![session.app.logs.level()];
            for _ in 0..4 {
                session.send(event)?;
                seen.push(session.app.logs.level());
            }
            let expected = [
                Level::Debug,
                Level::Info,
                Level::Warn,
                Level::Error,
                Level::Debug,
            ];
            assert_eq!(seen, expected);
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::Logs,
        "p",
        Some(KeyAction::CyclePhase),
        "filters to the next phase seen, then to all",
        |event| {
            let app = feed(on(Screen::Logs), 1, running());
            let mut session = Session::new(app);
            assert_eq!(session.app.logs.only_phase(), None);
            session.send(event)?;
            assert_eq!(session.app.logs.only_phase(), Some(Phase::Green));
            session.send(event)?;
            assert_eq!(session.app.logs.only_phase(), None);
            Ok(())
        },
    )?);
    // Errors at entries 1 and 4 of six; the cursor starts on the newest.
    for (name, via, expected) in [
        ("e", KeyAction::NextError, 1),
        ("E", KeyAction::PrevError, 4),
    ] {
        cases.push(case(
            Screen::Logs,
            name,
            Some(via),
            "goes to the next or previous error, wrapping",
            move |event| {
                let failure = EventKind::TaskFailed {
                    class: FailureClass::AgentFailure,
                    detail: "gave up".to_owned(),
                };
                let kinds = vec![
                    queued("a"),
                    failure.clone(),
                    queued("b"),
                    queued("c"),
                    failure,
                    queued("d"),
                ];
                let mut session = Session::new(feed(on(Screen::Logs), 1, kinds));
                assert_eq!(selected_entry(&session.app), 5);
                session.send(event)?;
                assert_eq!(selected_entry(&session.app), expected);
                let level = session
                    .app
                    .logs
                    .selected()
                    .map(ktask_tui::screen::logs::Entry::level);
                assert_eq!(level, Some(Level::Error));
                Ok(())
            },
        )?);
    }
    logs_search_cases(cases)
}

// ---- failures ----

fn failures_cases(cases: &mut Vec<Case>) -> Fallible {
    let failed = || feed(on(Screen::Failures), 1, failed_run());
    cases.push(dispatches(
        Screen::Failures,
        "r",
        "asks for the failed task to be retried",
        failed,
        vec![Action::Retry { task: task(1) }],
    )?);
    cases.push(dispatches(
        Screen::Failures,
        "c",
        "asks for the failed task to be cancelled",
        failed,
        vec![Action::Cancel { task: task(1) }],
    )?);
    cases.push(dispatches(
        Screen::Failures,
        "x",
        "asks for the failed task's gates to be run again",
        failed,
        vec![Action::RerunGate {
            task: task(1),
            gate: None,
        }],
    )?);
    Ok(())
}

// ---- inspector ----

/// The inspector on a task that has had two attempts.
fn two_attempts() -> Session {
    let kinds = vec![
        queued("Add the parser"),
        started(1, "tdd"),
        started(2, "tdd"),
    ];
    Session::new(feed(on(Screen::Inspector), 1, kinds))
}

fn inspector_cases(cases: &mut Vec<Case>) -> Fallible {
    cases.push(case(
        Screen::Inspector,
        "[",
        Some(KeyAction::PrevAttempt),
        "shows the previous attempt",
        |event| {
            let mut session = two_attempts();
            assert_eq!(session.app.inspector.viewing(task(1)), Some(1));
            session.send(event)?;
            assert_eq!(session.app.inspector.viewing(task(1)), Some(0));
            Ok(())
        },
    )?);
    cases.push(case(
        Screen::Inspector,
        "]",
        Some(KeyAction::NextAttempt),
        "shows the next attempt",
        |event| {
            let mut session = two_attempts();
            session.press("[")?;
            assert_eq!(session.app.inspector.viewing(task(1)), Some(0));
            session.send(event)?;
            assert_eq!(session.app.inspector.viewing(task(1)), Some(1));
            Ok(())
        },
    )?);
    Ok(())
}

// ---- input inbox ----

/// The inbox with three questions, the second one selected.
fn inbox_on_second() -> Fallible<Session> {
    let app = (1..=3).fold(on(Screen::InputInbox), |app, id| feed(app, id, asking()));
    let mut session = Session::new(app);
    session.press("j")?;
    assert_eq!(session.app.inbox.selected(), Some(task(2)));
    Ok(session)
}

fn inbox_cases(cases: &mut Vec<Case>) -> Fallible {
    for name in ["a", "Enter"] {
        cases.push(on_session(
            Screen::InputInbox,
            name,
            None,
            "starts an empty answer to the selected question",
            inbox_on_second,
            |session| assert_eq!(session.app.inbox.draft(), Some((task(2), ""))),
        )?);
    }
    cases.push(on_session(
        Screen::InputInbox,
        "r",
        None,
        "starts an answer from the recommended response",
        inbox_on_second,
        |session| assert_eq!(session.app.inbox.draft(), Some((task(2), "Reject them"))),
    )?);
    Ok(())
}

// ---- history ----

/// The `R` of the heading's "line R of N": the row the view is on.
fn history_row(text: &str) -> usize {
    numbers(text, "line ")
        .first()
        .map_or(0, |row| usize::try_from(*row).unwrap_or(0))
}

/// The history screen over a journal holding one task's long run.
fn history_session() -> Fallible<Session> {
    let dir = tempfile::tempdir()?;
    let mut journal = Journal::open(&dir.path().join("journal.db"))?;
    let mut kinds = vec![queued("Add the parser"), started(1, "tdd")];
    kinds.extend((0..100).map(|n| phase(if n % 2 == 0 { Phase::Red } else { Phase::Green })));
    let mut app = on(Screen::History);
    for (kind, n) in kinds.into_iter().zip(1..) {
        journal.append(Some(task(1)), &kind)?;
        app = update(app, AppEvent::Core(event(n, 1, kind)));
    }
    Session::reading(app, vec![Box::new(dir)], move |app| {
        while history::backfill(app, &journal)? {}
        Ok(())
    })
}

/// The history screen scrolled part of the way down from the first row.
fn history_in_the_middle() -> Fallible<(Session, usize, usize)> {
    let mut session = history_session()?;
    session.press("g")?;
    let top = history_row(&session.text());
    session.press("PgDn")?;
    let screenful = history_row(&session.text()) - top;
    session.press("PgDn")?;
    Ok((session, top + 2 * screenful, screenful))
}

fn history_cases(cases: &mut Vec<Case>) -> Fallible {
    for name in MOVEMENT {
        cases.push(case(
            Screen::History,
            name,
            moves(name),
            "moves the view a row, or to either end",
            move |event| {
                let (mut session, middle, _) = history_in_the_middle()?;
                assert_eq!(history_row(&session.text()), middle);
                let total = session.app.history.len();
                session.send(event)?;
                let (row, following) = (
                    history_row(&session.text()),
                    session.app.history.is_following(),
                );
                match name {
                    "j" | "Down" => assert_eq!((row, following), (middle + 1, false)),
                    "k" | "Up" => assert_eq!((row, following), (middle - 1, false)),
                    "g" => assert_eq!((row, following), (1, false)),
                    _ => assert_eq!((row, following), (total, true), "G follows the newest row"),
                }
                Ok(())
            },
        )?);
    }
    for name in ["PgUp", "PgDn"] {
        cases.push(case(
            Screen::History,
            name,
            None,
            "scrolls a screenful",
            move |event| {
                let (mut session, middle, screenful) = history_in_the_middle()?;
                assert!(screenful > 1, "a screenful is more than a row");
                session.send(event)?;
                let expected = if name == "PgUp" {
                    middle - screenful
                } else {
                    middle + screenful
                };
                assert_eq!(history_row(&session.text()), expected);
                Ok(())
            },
        )?);
    }
    cases.push(case(
        Screen::History,
        "t",
        None,
        "switches between the whole queue's timeline and the selected task's",
        |event| {
            let mut session = history_session()?;
            let whole = session.text();
            assert!(!session.app.history.is_task_scope());
            assert!(whole.contains("whole queue"));
            session.send(event)?;
            assert!(session.app.history.is_task_scope());
            assert!(session.text().contains("task 1"));
            session.send(event)?;
            assert!(!session.app.history.is_task_scope());
            assert!(session.text().contains("whole queue"));
            Ok(())
        },
    )?);
    Ok(())
}

// ---- git ----

/// A git command run with the identity and dates pinned.
fn git_in(dir: &Path, args: &[&str]) -> Fallible<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2026-09-23T10:30:00Z")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_DATE", "2026-09-23T10:30:00Z")
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn commit(work: &Path, message: &str) -> Fallible {
    git_in(work, &["add", "-A"])?;
    git_in(work, &["commit", "--quiet", "-m", message])?;
    Ok(())
}

/// A clone whose task changed five files, one of them with a diff of two
/// hundred lines (`line_001` to `line_200`), and the commit it started from.
fn repo_with_five_changes() -> Fallible<(tempfile::TempDir, Repo, String)> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let work = root.join("work");
    git_in(root, &["init", "--quiet", "work"])?;
    git_in(&work, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    for name in ["parser.rs", "old.rs", "kept.rs"] {
        std::fs::write(work.join(name), "fn seed() {}\n")?;
    }
    commit(&work, "Seed")?;
    let base = git_in(&work, &["rev-parse", "HEAD"])?;
    let numbered: Vec<String> = (1..=200).map(|n| format!("line_{n:03}")).collect();
    let long = numbered.join("\n") + "\n";
    std::fs::write(work.join("a_long.rs"), long)?;
    std::fs::write(work.join("block.rs"), "pub struct Block;\n")?;
    std::fs::write(work.join("parser.rs"), "fn seed() {\n    block();\n}\n")?;
    std::fs::write(work.join("z_last.rs"), "fn last() {}\n")?;
    std::fs::remove_file(work.join("old.rs"))?;
    commit(&work, "Change five files")?;
    let repo = Repo {
        worktree: work,
        remote: "origin".to_owned(),
        branch: "main".to_owned(),
    };
    Ok((dir, repo, base))
}

/// The git screen on the task that made those changes.
fn git_session() -> Fallible<Session> {
    let (dir, repo, base) = repo_with_five_changes()?;
    let mut app = on(Screen::Git);
    app = update(app, AppEvent::Core(event(1, 1, queued("Add the parser"))));
    app = update(
        app,
        AppEvent::Core(event(
            2,
            1,
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "tdd".to_owned(),
                pid: 4242,
                base_sha: base,
            },
        )),
    );
    let session = Session::reading(app, vec![Box::new(dir)], move |app| {
        while git::backfill(app, &|_| Some(repo.clone())) {}
        Ok(())
    })?;
    assert_eq!(session.app.git.file_count(), FIVE);
    Ok(session)
}

fn git_cases(cases: &mut Vec<Case>) -> Fallible {
    for name in ["PgDn", "PgUp"] {
        cases.push(case(
            Screen::Git,
            name,
            None,
            "scrolls the selected file's diff a screenful",
            move |event| {
                let mut session = git_session()?;
                let shown = numbers(&session.text(), "line_");
                assert_eq!(shown.first(), Some(&1), "the long diff is the first file's");
                if name == "PgUp" {
                    session.press("PgDn")?;
                    session.press("PgDn")?;
                }
                let before = numbers(&session.text(), "line_");
                session.send(event)?;
                let after = numbers(&session.text(), "line_");
                let (Some(was), Some(now)) = (before.first(), after.first()) else {
                    return Err("the diff shows no lines".into());
                };
                if name == "PgDn" {
                    assert!(now > was, "{was} -> {now}");
                    assert_eq!(*now, before.last().copied().unwrap_or(0) + 1);
                } else {
                    assert!(now < was, "{was} -> {now}");
                }
                Ok(())
            },
        )?);
    }
    Ok(())
}

// ---- configuration ----

fn setting(number: u32) -> Setting {
    Setting {
        key: format!("setting_{number:03}"),
        value: Some(number.to_string()),
        source: Source::Default,
    }
}

fn configuration_snapshot() -> Snapshot {
    Snapshot {
        settings: (1..=100).map(setting).collect(),
        checks: vec![CheckResult {
            check: "git",
            status: CheckStatus::Pass,
            detail: "git 2.51.0".to_owned(),
            remedy: None,
        }],
    }
}

/// The configuration screen, and how many times the configuration was read.
fn config_session() -> Fallible<(Session, Rc<Cell<u32>>)> {
    let reads = Rc::new(Cell::new(0));
    let counted = Rc::clone(&reads);
    let session = Session::reading(on(Screen::Config), Vec::new(), move |app| {
        config::backfill(app, &|| {
            counted.set(counted.get() + 1);
            Ok(configuration_snapshot())
        });
        Ok(())
    })?;
    Ok((session, reads))
}

/// The configuration screen scrolled two screenfuls down.
fn config_in_the_middle() -> Fallible<Session> {
    let (mut session, _) = config_session()?;
    session.press_all(&["PgDn", "PgDn"])?;
    Ok(session)
}

fn config_cases(cases: &mut Vec<Case>) -> Fallible {
    cases.push(case(
        Screen::Config,
        "r",
        None,
        "reads the configuration and runs the checks again",
        |event| {
            let (mut session, reads) = config_session()?;
            assert_eq!(reads.get(), 1);
            assert!(!session.app.config.refreshing());
            session.send(event)?;
            assert_eq!(reads.get(), 2, "the screen was read again");
            assert!(!session.app.config.refreshing());
            Ok(())
        },
    )?);
    for name in MOVEMENT {
        cases.push(case(
            Screen::Config,
            name,
            moves(name),
            "scrolls a line, or to either end",
            move |event| {
                let mut session = config_in_the_middle()?;
                let before = numbers(&session.text(), "setting_");
                let first = before.first().copied().unwrap_or(0);
                assert!(first > 10, "the setup did not scroll: {before:?}");
                session.send(event)?;
                let after = numbers(&session.text(), "setting_");
                let (was, now) = (first, after.first().copied().unwrap_or(0));
                match name {
                    "j" | "Down" => assert_eq!(now, was + 1),
                    "k" | "Up" => assert_eq!(now, was - 1),
                    "g" => {
                        assert_eq!(now, 1);
                        assert!(
                            session.text().contains("git 2.51.0"),
                            "the doctor is on top"
                        );
                    }
                    _ => assert_eq!(after.last(), Some(&100)),
                }
                Ok(())
            },
        )?);
    }
    for name in ["PgUp", "PgDn"] {
        cases.push(case(
            Screen::Config,
            name,
            None,
            "scrolls a screenful",
            move |event| {
                let mut session = config_in_the_middle()?;
                let before = numbers(&session.text(), "setting_");
                let (first, last) = (before.first().copied(), before.last().copied());
                session.send(event)?;
                let after = numbers(&session.text(), "setting_");
                let (Some(first), Some(last)) = (first, last) else {
                    return Err("the screen shows no settings".into());
                };
                if name == "PgDn" {
                    assert_eq!(after.first(), Some(&(last + 1)));
                } else {
                    assert_eq!(after.last(), Some(&(first - 1)));
                }
                Ok(())
            },
        )?);
    }
    Ok(())
}

// ---- all of them ----

fn cases() -> Fallible<Vec<Case>> {
    let mut all = Vec::new();
    every_screen_cases(&mut all)?;
    search_cases(&mut all)?;
    movement_cases(&mut all)?;
    queue_actions(&mut all)?;
    queue_views(&mut all)?;
    live_cases(&mut all)?;
    logs_cases(&mut all)?;
    failures_cases(&mut all)?;
    inspector_cases(&mut all)?;
    inbox_cases(&mut all)?;
    history_cases(&mut all)?;
    git_cases(&mut all)?;
    config_cases(&mut all)?;
    Ok(all)
}

/// The key as an operator would name it, for a message.
fn named((code, modifiers): Key) -> String {
    if modifiers.is_empty() {
        format!("{code:?}")
    } else {
        format!("{code:?} with {modifiers:?}")
    }
}

/// The documented keys with no case, and the cases for keys not documented.
fn compare(documented: &[Documented], cases: &[Case]) -> (Vec<String>, Vec<String>) {
    let missing = documented
        .iter()
        .filter(|doc| {
            !cases
                .iter()
                .any(|case| (case.screen, case.key) == (doc.screen, doc.key))
        })
        .map(|doc| format!("{:?} on {:?}: `{}`", doc.key, doc.screen, doc.line))
        .collect();
    let stale = cases
        .iter()
        .filter(|case| {
            !documented
                .iter()
                .any(|doc| (doc.screen, doc.key) == (case.screen, case.key))
        })
        .map(|case| format!("{} on {:?}: {}", named(case.key), case.screen, case.what))
        .collect();
    (missing, stale)
}

#[test]
fn keys_every_documented_binding_has_a_case() -> Fallible {
    let (missing, _) = compare(&documented()?, &cases()?);
    assert!(
        missing.is_empty(),
        "docs/CONTRACT.md documents keys that tests/keys.rs does not send: add a case for each:\n{}",
        missing.join("\n")
    );
    Ok(())
}

#[test]
fn keys_every_case_is_for_a_documented_binding() -> Fallible {
    let (_, stale) = compare(&documented()?, &cases()?);
    assert!(
        stale.is_empty(),
        "these cases are for keys docs/CONTRACT.md §4 does not document:\n{}",
        stale.join("\n")
    );
    Ok(())
}

/// The table is what the key map overlay lists and what the shared keys are
/// looked up in, so it has to say what the cases expect on every screen, and
/// name nothing the contract does not.
#[test]
fn keys_every_case_agrees_with_the_table() -> Fallible {
    let mut wrong = Vec::new();
    for case in cases()? {
        let event = KeyEvent::new(case.key.0, case.key.1);
        let found = lookup(case.screen, &event).map(|binding| binding.action);
        if found != case.via {
            wrong.push(format!(
                "{} on {:?}: the table gives {found:?}, the case expects {:?}",
                named(case.key),
                case.screen,
                case.via
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    Ok(())
}

/// Every row of the table, on each screen it names, is a documented key with
/// a case, and resolves to its own action however the terminal reports it.
#[test]
fn keys_every_binding_in_the_table_is_documented_and_sent() -> Fallible {
    let documented = documented()?;
    let cases = cases()?;
    let mut problems = Vec::new();
    for binding in &BINDINGS {
        let key = canonical(&KeyEvent::new(binding.key, binding.modifiers));
        for screen in binding.screens {
            if !documented
                .iter()
                .any(|doc| (doc.screen, doc.key) == (*screen, key))
            {
                problems.push(format!("{} on {screen:?} is not documented", named(key)));
            }
            if !cases
                .iter()
                .any(|case| (case.screen, case.key) == (*screen, key))
            {
                problems.push(format!("{} on {screen:?} has no case", named(key)));
            }
            for event in reports(key) {
                let found = lookup(*screen, &event).map(|found| found.action);
                if found != Some(binding.action) {
                    problems.push(format!("{event:?} on {screen:?} finds {found:?}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(())
}

/// The reason a case failed, from what it panicked with or returned.
fn reason(outcome: std::thread::Result<Fallible>) -> Option<String> {
    match outcome {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(err.to_string()),
        Err(payload) => Some(
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                })
                .unwrap_or_else(|| "panicked".to_owned()),
        ),
    }
}

/// Sends every case its key, in each way a terminal reports it, and gathers
/// the failures so that one broken key does not hide another.
#[test]
fn keys_every_case_passes() -> Fallible {
    let mut failures = Vec::new();
    for case in cases()? {
        for event in reports(case.key) {
            let outcome = catch_unwind(AssertUnwindSafe(|| (case.run)(&event)));
            if let Some(why) = reason(outcome) {
                failures.push(format!(
                    "{:?} ({event:?}) on {:?}, which {}:\n    {why}",
                    case.key, case.screen, case.what
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

// ---- the tests of the tests ----

/// A contract that names no key, or the wrong ones, would leave every check
/// above passing over nothing.
#[test]
fn keys_the_contract_is_read_for_every_key_block() -> Fallible {
    let documented = documented()?;
    for screen in Screen::ALL {
        let keys: Vec<Key> = documented
            .iter()
            .filter(|doc| doc.screen == screen)
            .map(|doc| doc.key)
            .collect();
        for digit in '1'..='9' {
            assert!(
                keys.contains(&key(&digit.to_string())?),
                "{screen:?} {digit}"
            );
        }
        for name in [
            "Tab", "S-Tab", "?", "F1", "/", "Esc", "j", "k", "Up", "Down", "g", "G", "Ctrl-C", "q",
        ] {
            assert!(keys.contains(&key(name)?), "{screen:?} {name}");
        }
    }
    let on = |screen: Screen, name: &str| -> Fallible<bool> {
        let wanted = key(name)?;
        Ok(documented
            .iter()
            .any(|doc| doc.screen == screen && doc.key == wanted))
    };
    for (screen, names) in [
        (Screen::Queue, "p i R r c A x a d Enter"),
        (Screen::LiveRun, "f"),
        (Screen::Logs, "v l p n N e E"),
        (Screen::Failures, "r c x"),
        (Screen::Inspector, "[ ]"),
        (Screen::InputInbox, "a Enter r"),
        (Screen::History, "PgUp PgDn t"),
        (Screen::Git, "PgUp PgDn"),
        (Screen::Config, "r PgUp PgDn"),
    ] {
        for name in names.split(' ') {
            assert!(on(screen, name)?, "{screen:?} {name}");
        }
    }
    // Keys of another screen's block are not read into this one.
    assert!(!on(Screen::LiveRun, "p")?);
    assert!(!on(Screen::Failures, "a")?);
    assert!(!on(Screen::Queue, "PgUp")?);
    Ok(())
}

#[test]
fn keys_a_binding_added_to_the_contract_is_found_and_reported_when_it_has_no_case() -> Fallible {
    let contract = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/CONTRACT.md"),
    )?;
    let extended = contract.replacen(
        "   p / i       pause / interrupt the run\n",
        "   p / i       pause / interrupt the run\n   w           a key nothing is bound to\n",
        1,
    );
    assert_ne!(extended, contract, "the queue's key block was not found");
    let (missing, _) = compare(&documented_in(&extended)?, &cases()?);
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(
        missing
            .iter()
            .any(|line| line.contains("Char('w')") && line.contains("Queue")),
        "{missing:?}"
    );
    Ok(())
}

#[test]
fn keys_a_binding_dropped_from_the_contract_leaves_its_case_reported_as_stale() -> Fallible {
    let contract = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/CONTRACT.md"),
    )?;
    let reduced = contract.replacen("   x           re-run its gates\n", "", 1);
    assert_ne!(reduced, contract, "the failures' key block was not found");
    let (_, stale) = compare(&documented_in(&reduced)?, &cases()?);
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(
        stale
            .iter()
            .any(|line| line.contains("Char('x')") && line.contains("Failures")),
        "{stale:?}"
    );
    Ok(())
}

#[test]
fn keys_the_names_the_contract_uses_are_understood() -> Fallible {
    assert_eq!(key("Ctrl-C")?, (KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(key("S-Tab")?, (KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(key("G")?, (KeyCode::Char('G'), KeyModifiers::NONE));
    assert_eq!(key("dn")?, (KeyCode::Down, KeyModifiers::NONE));
    assert!(key("Hyper-X").is_err());
    assert!(key("Ctrl-CC").is_err());
    let spec = |line: &str| keys_of(line).map(|(keys, rest)| (keys.len(), rest));
    assert_eq!(
        spec("j/k, up/dn   move selection")?,
        (4, "move selection".to_owned())
    );
    assert_eq!(
        spec("? or F1      key map overlay")?,
        (2, "key map overlay".to_owned())
    );
    assert_eq!(
        spec("1..9        jump to screen")?,
        (9, "jump to screen".to_owned())
    );
    assert_eq!(
        spec("/           type a search")?,
        (1, "type a search".to_owned())
    );
    assert_eq!(
        keys_of("[ / ]       show the previous")?.0,
        vec![key("[")?, key("]")?]
    );
    assert_eq!(
        spec("Tab / S-Tab next / previous screen")?,
        (2, "next / previous screen".to_owned())
    );
    assert_eq!(
        spec("PgUp / PgDn scroll a screenful")?,
        (2, "scroll a screenful".to_owned())
    );
    assert_eq!(spec("Ctrl-C      quit")?, (1, "quit".to_owned()));
    Ok(())
}

/// A case that could not fail would leave its key unproved; the runner has to
/// see a panic and an error for what they are.
#[test]
fn keys_a_failing_case_is_reported_with_its_reason() {
    let panicked = catch_unwind(|| -> Fallible { panic!("the selection did not move") });
    assert_eq!(
        reason(panicked).as_deref(),
        Some("the selection did not move")
    );
    let failed: std::thread::Result<Fallible> = Ok(Err("no repository".into()));
    assert_eq!(reason(failed).as_deref(), Some("no repository"));
    let passed: std::thread::Result<Fallible> = Ok(Ok(()));
    assert_eq!(reason(passed), None);
}

/// The queue's action keys read a task's state; a case that dispatched from
/// the wrong state would prove nothing about the key.
#[test]
fn keys_the_queue_refuses_an_action_in_the_wrong_state_and_says_why() -> Fallible {
    let mut session = Session::new(queue_of(&["Done", "Queued"]));
    session.press("p")?;
    assert!(session.app.outbox.is_empty());
    assert!(session.app.notice.is_some());
    Ok(())
}

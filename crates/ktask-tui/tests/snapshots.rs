//! A snapshot of every screen in each of four states, at 80x24.
//!
//! The unit tests beside each screen pin individual behaviors; these pin the
//! whole picture an operator sees, so a change to layout, wording or a shared
//! header shows up as a reviewed diff of the screen, not as a red assertion
//! somewhere unrelated.
//!
//! The states are what a screen can be in when the interface starts, reads and
//! fails:
//!
//! - **empty**: nothing has happened; the screen says so.
//! - **loading**: the screen has not got what it shows yet. Where it has a
//!   marker for that (`Reading the journal…`, `Reading the repository…`,
//!   `older output, not loaded`, the inspector's `definition is not loaded`) the
//!   snapshot shows it. The queue, logs, failures and inbox fold the journal as
//!   events arrive and have no such marker, so theirs is the first thing a run
//!   gives them: tasks that are queued, a run that has begun and has nothing
//!   yet to show. The failures board and the inbox look the same then as when
//!   empty, which the tests below allow for those two states alone.
//! - **populated**: a run part of the way through, with something on the screen.
//! - **error**: what the screen shows when something went wrong: a read that
//!   failed (configuration, git), a failed run, or the notice that says why the
//!   operator's key did nothing.
//!
//! Every state is reached through the public API, the way the shell reaches it:
//! journal events through [`update`] and the reads through each screen's
//! `backfill`. [`snapshots_every_screen_has_a_snapshot_in_every_state`] fails
//! when a [`Screen`] has one missing, which is how a new screen is made to
//! bring its snapshots with it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{
    AttemptId, CheckResult, CheckStatus, DecisionRequest, Event, EventKind, EventSeq, FailureClass,
    GateKind, GateResult, Journal, Phase, Setting, Source, Stream, Task, TaskId, TaskStatus,
};
use ktask_tui::screen::config::{self, Snapshot};
use ktask_tui::screen::git::{self, Repo};
use ktask_tui::screen::{history, title};
use ktask_tui::testing::Harness;
use ktask_tui::{App, AppEvent, Screen, TaskView, update};
use std::path::{Path, PathBuf};
use std::process::Command;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const SIZE: (u16, u16) = (80, 24);

/// What a helper that can fail returns, so that the failure reaches the test
/// that asked for the state and not a panic in a helper.
type Fallible<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The four states every screen is snapshotted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Empty,
    Loading,
    Populated,
    Error,
}

impl State {
    const ALL: [State; 4] = [State::Empty, State::Loading, State::Populated, State::Error];

    fn name(self) -> &'static str {
        match self {
            State::Empty => "empty",
            State::Loading => "loading",
            State::Populated => "populated",
            State::Error => "error",
        }
    }
}

/// The name a screen's snapshots carry: its title, lowercased, with a hyphen
/// between words. Derived from [`title`] rather than listed, so a new screen
/// is expected to have snapshots without anyone remembering to say so.
fn slug(screen: Screen) -> String {
    title(screen).to_lowercase().replace(' ', "-")
}

fn snapshot_name(screen: Screen, state: State) -> String {
    format!("{}_{}", slug(screen), state.name())
}

// ---- the harness ----

/// The rows as drawn, each without its trailing blanks.
fn rows(app: &App) -> String {
    Harness::from_app(app.clone())
        .text()
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replaces every date (`yyyy-mm-dd`) and time (`hh:mm:ss`) in `text` with
/// dashes, digit for digit.
///
/// The journal stamps the events it appends with the moment it appended them,
/// which a test cannot set, so the history read from a real journal is masked
/// before it is compared. Every other screen is fed events with the fixed times
/// of [`event`] and is compared as drawn.
fn mask_clock(text: &str) -> String {
    /// `d` is a digit; anything else stands for itself.
    const SHAPES: [&str; 2] = ["dddd-dd-dd", "dd:dd:dd"];
    let chars: Vec<char> = text.chars().collect();
    let shape_at = |at: usize| {
        SHAPES.iter().find(|shape| {
            chars.get(at..at + shape.len()).is_some_and(|window| {
                window
                    .iter()
                    .zip(shape.chars())
                    .all(|(c, want)| match want {
                        'd' => c.is_ascii_digit(),
                        literal => *c == literal,
                    })
            })
        })
    };
    let mut masked = String::new();
    let mut at = 0;
    while let Some(c) = chars.get(at) {
        if let Some(shape) = shape_at(at) {
            masked.extend(shape.chars().map(|c| if c == 'd' { '-' } else { c }));
            at += shape.len();
        } else {
            masked.push(*c);
            at += 1;
        }
    }
    masked
}

/// Draws `app` and compares it with the committed snapshot of `screen` in
/// `state`, after checking it is on that screen and shows every one of
/// `expect`: a state that was built wrong (an empty screen where a populated
/// one was meant) fails here, not in a snapshot nobody reads closely.
fn check(screen: Screen, state: State, app: &App, expect: &[&str]) {
    check_drawn(screen, state, app, &rows(app), expect);
}

/// [`check`], for a screen whose `drawn` text was adjusted after drawing.
fn check_drawn(screen: Screen, state: State, app: &App, drawn: &str, expect: &[&str]) {
    assert_eq!(
        app.screen, screen,
        "the state was built on the wrong screen"
    );
    assert_eq!(app.size, SIZE);
    assert_eq!(drawn.split('\n').count(), usize::from(SIZE.1));
    for needle in expect {
        assert!(
            drawn.contains(needle),
            "{screen:?} in {state:?} does not show {needle:?}:\n{drawn}"
        );
    }
    insta::assert_snapshot!(snapshot_name(screen, state), drawn);
}

// ---- building states ----

fn on(screen: Screen) -> App {
    App {
        screen,
        ..App::new(SIZE)
    }
}

/// The instant of the `n`th fixture event: ten seconds apart, so durations
/// on screen are round.
fn at(n: u32) -> OffsetDateTime {
    datetime!(2026-09-23 10:30:00 UTC) + Duration::seconds(10 * i64::from(n))
}

fn event(n: u32, task: Option<u32>, kind: EventKind) -> Event {
    Event {
        seq: EventSeq::new(u64::from(n)),
        ts: at(n),
        task_id: task.map(TaskId::new),
        kind,
    }
}

/// Folds `kinds`, each for `task`, into `app` as journal events.
fn feed(app: App, task: u32, kinds: Vec<EventKind>) -> App {
    kinds.into_iter().zip(1..).fold(app, |app, (kind, n)| {
        update(app, AppEvent::Core(event(n, Some(task), kind)))
    })
}

fn press(app: App, code: KeyCode) -> App {
    update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn key(app: App, c: char) -> App {
    press(app, KeyCode::Char(c))
}

fn queued(title: &str) -> EventKind {
    EventKind::TaskQueued {
        title: title.to_owned(),
    }
}

fn attempt() -> AttemptId {
    AttemptId::new(1)
}

fn started(protocol: &str) -> EventKind {
    EventKind::AttemptStarted {
        attempt: attempt(),
        protocol: protocol.to_owned(),
        pid: 4242,
        base_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
    }
}

fn phase(phase: Phase) -> EventKind {
    EventKind::PhaseEntered {
        attempt: attempt(),
        phase,
    }
}

fn output(stream: Stream, text: &str) -> EventKind {
    EventKind::AgentOutput {
        attempt: attempt(),
        stream,
        text: text.to_owned(),
    }
}

fn gate(kind: GateKind, passed: bool, stderr: &str) -> GateResult {
    GateResult {
        kind,
        passed,
        exit_code: Some(i32::from(!passed)),
        signal: None,
        duration_ms: 2_400,
        stdout: String::new(),
        stderr: stderr.to_owned(),
        timed_out: false,
    }
}

fn gate_done(kind: GateKind, passed: bool, stderr: &str) -> EventKind {
    EventKind::GateFinished {
        result: gate(kind, passed, stderr),
    }
}

/// Everything up to an agent working in `phase` of a `protocol` attempt.
fn running(protocol: &str, phase_now: Phase) -> Vec<EventKind> {
    vec![
        queued("Add the parser"),
        EventKind::PreflightStarted,
        EventKind::PreflightPassed {
            base_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        },
        started(protocol),
        phase(phase_now),
    ]
}

/// A run whose verification failed and whose task then failed.
fn failed_run() -> Vec<EventKind> {
    let mut kinds = running("tdd", Phase::Green);
    kinds.extend([
        output(Stream::Stdout, "running the tests\n"),
        output(Stream::Stderr, "error[E0308]: mismatched types\n"),
        EventKind::AttemptFinished {
            attempt: attempt(),
            exit_code: 1,
            usage: None,
            session_id: None,
            model_reported: None,
        },
        gate_done(GateKind::Verify, false, "error[E0308]: mismatched types"),
        EventKind::VerifyFailed {
            attempt: attempt(),
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

/// A task waiting for the operator's answer.
fn asking() -> Vec<EventKind> {
    let mut kinds = running("tdd", Phase::Implement);
    kinds.extend([
        EventKind::DecisionRaised {
            request: DecisionRequest {
                question: "Should the parser reject trailing commas?".to_owned(),
                options: vec!["Reject them".to_owned(), "Accept them".to_owned()],
                tradeoffs: "Rejecting is stricter; accepting is friendlier".to_owned(),
                impact: "Every task file already written".to_owned(),
                recommended: Some("Reject them".to_owned()),
            },
        },
        EventKind::Paused {
            reason: ktask_core::PauseReason::Input,
        },
    ]);
    kinds
}

fn task_definition(id: u32) -> Task {
    Task {
        id: TaskId::new(id),
        status: TaskStatus::Pending,
        body: "Add the parser\n\n**Outcome:** plan files parse.".to_owned(),
        outcome: "Plan files parse into tasks.".to_owned(),
        done_when: "The parser round-trips every example plan.".to_owned(),
        verify: "cargo nextest run -p ktask-core".to_owned(),
        refs: "docs/CONTRACT.md section 2".to_owned(),
        protocol: Some("tdd".to_owned()),
    }
}

// ---- queue ----

/// A row of the queue as the shell builds it.
fn row(id: u32, state: &str, phase: Option<Phase>, attempts: u32, secs: Option<u64>) -> TaskView {
    TaskView {
        id: TaskId::new(id),
        title: format!("Task number {id}"),
        state: state.to_owned(),
        protocol: if attempts == 0 { "" } else { "tdd" }.to_owned(),
        phase,
        attempts,
        elapsed: secs.map(std::time::Duration::from_secs),
    }
}

fn queue_of(tasks: Vec<TaskView>) -> App {
    App {
        tasks,
        ..on(Screen::Queue)
    }
}

#[test]
fn snapshots_queue_empty() {
    check(
        Screen::Queue,
        State::Empty,
        &on(Screen::Queue),
        &["No tasks queued."],
    );
}

#[test]
fn snapshots_queue_loading() {
    // The plan has been read and nothing has started.
    let app = queue_of((1..=3).map(|id| row(id, "Queued", None, 0, None)).collect());
    check(Screen::Queue, State::Loading, &app, &["Queued"]);
}

#[test]
fn snapshots_queue_populated() {
    let app = queue_of(vec![
        row(1, "Done", None, 1, Some(312)),
        row(2, "Running", Some(Phase::Green), 2, Some(95)),
        row(3, "Queued", None, 0, None),
    ]);
    check(
        Screen::Queue,
        State::Populated,
        &app,
        &["Done", "Running", "Green", "Queued"],
    );
}

#[test]
fn snapshots_queue_error() {
    let app = queue_of(vec![
        row(1, "Done", None, 1, Some(312)),
        row(2, "Failed", Some(Phase::Green), 3, Some(640)),
        row(3, "Queued", None, 0, None),
    ]);
    // Nothing is running, so pausing is refused and the queue says why.
    let app = key(press(app, KeyCode::Down), 'p');
    check(
        Screen::Queue,
        State::Error,
        &app,
        &["Failed", "pause: no task is running"],
    );
}

// ---- live run ----

#[test]
fn snapshots_live_run_empty() {
    check(
        Screen::LiveRun,
        State::Empty,
        &on(Screen::LiveRun),
        &["No run in progress"],
    );
}

#[test]
fn snapshots_live_run_loading() {
    // The ring holds the newest four lines and the view is scrolled up to
    // older ones, which are read back from the journal when the shell gets to
    // it: the screen says they are not loaded yet.
    let mut app = on(Screen::LiveRun);
    ktask_tui::screen::live::set_cap(&mut app, 4);
    let mut kinds = running("tdd", Phase::Green);
    kinds.extend((1..=10).map(|n| output(Stream::Stdout, &format!("line {n}\n"))));
    let app = feed(app, 1, kinds);
    let app = (0..8).fold(app, |app, _| key(app, 'k'));
    check(
        Screen::LiveRun,
        State::Loading,
        &app,
        &["older output, not loaded"],
    );
}

#[test]
fn snapshots_live_run_populated() {
    let mut kinds = running("tdd", Phase::Green);
    kinds.extend([
        output(Stream::Stdout, "reading src/parser.rs\n"),
        output(Stream::Stdout, "adding the Block type\n"),
        gate_done(GateKind::Targeted, true, ""),
        output(Stream::Stdout, "running the targeted tests\n"),
    ]);
    let app = feed(on(Screen::LiveRun), 1, kinds);
    check(
        Screen::LiveRun,
        State::Populated,
        &app,
        &["adding the Block type", "targeted passed"],
    );
}

#[test]
fn snapshots_live_run_error() {
    let app = feed(on(Screen::LiveRun), 1, failed_run());
    check(
        Screen::LiveRun,
        State::Error,
        &app,
        &["error[E0308]: mismatched types", "verify failed"],
    );
}

// ---- logs ----

#[test]
fn snapshots_logs_empty() {
    check(
        Screen::Logs,
        State::Empty,
        &on(Screen::Logs),
        &["No log entries"],
    );
}

#[test]
fn snapshots_logs_loading() {
    let app = feed(
        on(Screen::Logs),
        1,
        vec![queued("Add the parser"), EventKind::PreflightStarted],
    );
    check(Screen::Logs, State::Loading, &app, &["preflight"]);
}

#[test]
fn snapshots_logs_populated() {
    let mut kinds = running("tdd", Phase::Green);
    kinds.extend([
        output(Stream::Stdout, "reading src/parser.rs\n"),
        gate_done(GateKind::Targeted, true, ""),
    ]);
    let app = feed(on(Screen::Logs), 1, kinds);
    check(
        Screen::Logs,
        State::Populated,
        &app,
        &["reading src/parser.rs"],
    );
}

#[test]
fn snapshots_logs_error() {
    let app = feed(on(Screen::Logs), 1, failed_run());
    check(
        Screen::Logs,
        State::Error,
        &app,
        &["mismatched types", "verify"],
    );
}

// ---- failures ----

#[test]
fn snapshots_failures_empty() {
    check(
        Screen::Failures,
        State::Empty,
        &on(Screen::Failures),
        &["No failures recorded."],
    );
}

#[test]
fn snapshots_failures_loading() {
    // A run has started and nothing has failed yet: the board has nothing to
    // show but is already listening.
    let app = feed(on(Screen::Failures), 1, running("tdd", Phase::Red));
    check(
        Screen::Failures,
        State::Loading,
        &app,
        &["No failures recorded."],
    );
}

#[test]
fn snapshots_failures_populated() {
    let mut app = feed(on(Screen::Failures), 1, failed_run());
    app = feed(
        app,
        2,
        vec![
            queued("Publish the results"),
            EventKind::PreflightStarted,
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "git is not installed".to_owned(),
            },
        ],
    );
    check(
        Screen::Failures,
        State::Populated,
        &app,
        &["verification", "git is not installed"],
    );
}

#[test]
fn snapshots_failures_error() {
    // The same signature three times over trips the circuit breaker.
    let mut app = on(Screen::Failures);
    for round in 1..=3 {
        let mut kinds = if round == 1 {
            running("tdd", Phase::Green)
        } else {
            // The remediation's next attempt: its gate results start afresh,
            // its failure counts do not.
            vec![EventKind::AttemptStarted {
                attempt: AttemptId::new(round),
                protocol: "tdd".to_owned(),
                pid: 4242,
                base_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            }]
        };
        kinds.extend([
            gate_done(GateKind::Verify, false, "error[E0308]: mismatched types"),
            EventKind::VerifyFailed {
                attempt: AttemptId::new(round),
                class: FailureClass::VerificationFailure,
                detail: "the verify gate failed with exit code 1".to_owned(),
            },
        ]);
        app = feed(app, 1, kinds);
    }
    check(
        Screen::Failures,
        State::Error,
        &app,
        &["CIRCUIT BREAKER TRIPPED"],
    );
}

// ---- task inspector ----

#[test]
fn snapshots_task_inspector_empty() {
    check(
        Screen::Inspector,
        State::Empty,
        &on(Screen::Inspector),
        &["No task to inspect."],
    );
}

#[test]
fn snapshots_task_inspector_loading() {
    let app = feed(on(Screen::Inspector), 1, vec![queued("Add the parser")]);
    check(
        Screen::Inspector,
        State::Loading,
        &app,
        &["the task's definition is not loaded", "No attempts yet."],
    );
}

#[test]
fn snapshots_task_inspector_populated() {
    let mut app = feed(on(Screen::Inspector), 1, running("tdd", Phase::Green));
    app.inspector.set_definition(&task_definition(1));
    app.tasks[0].state = "Running".to_owned();
    check(
        Screen::Inspector,
        State::Populated,
        &app,
        &["Plan files parse into tasks.", "in green"],
    );
}

#[test]
fn snapshots_task_inspector_error() {
    let mut app = feed(on(Screen::Inspector), 1, failed_run());
    app.inspector.set_definition(&task_definition(1));
    app.tasks[0].state = "Failed".to_owned();
    check(
        Screen::Inspector,
        State::Error,
        &app,
        &["Plan files parse into tasks.", "mismatched types"],
    );
}

// ---- input inbox ----

#[test]
fn snapshots_input_inbox_empty() {
    check(
        Screen::InputInbox,
        State::Empty,
        &on(Screen::InputInbox),
        &["No decisions are waiting."],
    );
}

#[test]
fn snapshots_input_inbox_loading() {
    // A task is being worked and has asked nothing yet.
    let app = feed(on(Screen::InputInbox), 1, running("tdd", Phase::Implement));
    check(
        Screen::InputInbox,
        State::Loading,
        &app,
        &["No decisions are waiting."],
    );
}

#[test]
fn snapshots_input_inbox_populated() {
    let app = feed(on(Screen::InputInbox), 1, asking());
    check(
        Screen::InputInbox,
        State::Populated,
        &app,
        &["Should the parser reject trailing commas?", "Reject them"],
    );
}

#[test]
fn snapshots_input_inbox_error() {
    // An empty answer is refused, and the inbox says so in place of sending it.
    let app = feed(on(Screen::InputInbox), 1, asking());
    let app = press(key(app, 'a'), KeyCode::Enter);
    assert!(app.outbox.is_empty(), "an empty answer must not be sent");
    check(
        Screen::InputInbox,
        State::Error,
        &app,
        &["Should the parser reject trailing commas?"],
    );
}

// ---- history ----

/// A journal in a temporary directory holding `events`, and the directory that
/// keeps it alive.
fn journal_of(events: &[(u32, EventKind)]) -> Fallible<(tempfile::TempDir, Journal)> {
    let dir = tempfile::tempdir()?;
    let mut journal = Journal::open(&dir.path().join("journal.db"))?;
    for (task, kind) in events {
        journal.append(Some(TaskId::new(*task)), kind)?;
    }
    Ok((dir, journal))
}

/// The history screen after reading a journal that holds `events`.
fn read_history(events: &[(u32, EventKind)]) -> Fallible<App> {
    let (_dir, journal) = journal_of(events)?;
    let mut app = on(Screen::History);
    for ((task, kind), n) in events.iter().zip(1..) {
        app = update(app, AppEvent::Core(event(n, Some(*task), kind.clone())));
    }
    assert!(history::backfill(&mut app, &journal)?);
    Ok(app)
}

fn check_history(state: State, app: &App, expect: &[&str]) {
    let drawn = mask_clock(&rows(app));
    check_drawn(Screen::History, state, app, &drawn, expect);
}

#[test]
fn snapshots_history_empty() -> Fallible {
    check_history(State::Empty, &read_history(&[])?, &["No events recorded"]);
    Ok(())
}

#[test]
fn snapshots_history_loading() {
    // The journal has events, and the screen has not read them yet.
    let app = feed(on(Screen::History), 1, running("tdd", Phase::Red));
    check_history(State::Loading, &app, &["Reading the journal…"]);
}

#[test]
fn snapshots_history_populated() -> Fallible {
    let events: Vec<(u32, EventKind)> = running("tdd", Phase::Green)
        .into_iter()
        .chain([gate_done(GateKind::Targeted, true, "")])
        .map(|kind| (1, kind))
        .collect();
    check_history(
        State::Populated,
        &read_history(&events)?,
        &["TaskQueued", "PhaseEntered"],
    );
    Ok(())
}

#[test]
fn snapshots_history_error() -> Fallible {
    let events: Vec<(u32, EventKind)> = failed_run().into_iter().map(|kind| (1, kind)).collect();
    check_history(
        State::Error,
        &read_history(&events)?,
        &["VerifyFailed", "TaskFailed"],
    );
    Ok(())
}

// ---- git ----

/// A git command run with the identity and dates pinned, so that the commits
/// it makes have the same hashes on every run and machine.
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

/// A clone with a bare remote, and two commits of the task's work on top of
/// the one the task started from. Returns the temporary directory, the repo
/// and the starting commit.
fn repo_with_work() -> Fallible<(tempfile::TempDir, Repo, String)> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (remote, work): (PathBuf, PathBuf) = (root.join("remote.git"), root.join("work"));
    git_in(root, &["init", "--quiet", "--bare", "remote.git"])?;
    git_in(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    git_in(root, &["init", "--quiet", "work"])?;
    git_in(&work, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    git_in(
        &work,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    )?;
    std::fs::write(work.join("parser.rs"), "fn parse() {}\n")?;
    std::fs::write(work.join("old.rs"), "fn old() {}\n")?;
    commit(&work, "Seed")?;
    git_in(&work, &["push", "--quiet", "origin", "main"])?;
    let base = git_in(&work, &["rev-parse", "HEAD"])?;
    std::fs::write(work.join("parser.rs"), "fn parse() {\n    block();\n}\n")?;
    std::fs::write(work.join("block.rs"), "pub struct Block;\n")?;
    commit(&work, "Add the Block type")?;
    std::fs::remove_file(work.join("old.rs"))?;
    commit(&work, "Drop the old parser")?;
    let repo = Repo {
        worktree: work,
        remote: "origin".to_owned(),
        branch: "main".to_owned(),
    };
    Ok((dir, repo, base))
}

/// The git screen on a task whose attempt started from `base`.
fn git_app(base: &str) -> App {
    let mut app = on(Screen::Git);
    app = update(
        app,
        AppEvent::Core(event(1, Some(1), queued("Add the parser"))),
    );
    update(
        app,
        AppEvent::Core(event(
            2,
            Some(1),
            EventKind::AttemptStarted {
                attempt: attempt(),
                protocol: "tdd".to_owned(),
                pid: 4242,
                base_sha: base.to_owned(),
            },
        )),
    )
}

#[test]
fn snapshots_git_empty() {
    check(
        Screen::Git,
        State::Empty,
        &on(Screen::Git),
        &["No task is selected"],
    );
}

#[test]
fn snapshots_git_loading() {
    let app = git_app("0123456789abcdef0123456789abcdef01234567");
    check(
        Screen::Git,
        State::Loading,
        &app,
        &["Reading the repository…"],
    );
}

#[test]
fn snapshots_git_populated() -> Fallible {
    let (_dir, repo, base) = repo_with_work()?;
    let mut app = git_app(&base);
    while git::backfill(&mut app, &|_| Some(repo.clone())) {}
    check(
        Screen::Git,
        State::Populated,
        &app,
        &["block.rs", "Add the Block type", "Drop the old parser"],
    );
    Ok(())
}

#[test]
fn snapshots_git_error() {
    let mut app = git_app("0123456789abcdef0123456789abcdef01234567");
    // The worktree has been removed since the attempt.
    assert!(git::backfill(&mut app, &|_| None));
    check(
        Screen::Git,
        State::Error,
        &app,
        &["The task's worktree is gone"],
    );
}

// ---- configuration and doctor ----

fn setting(key: &str, value: Option<&str>, source: Source) -> Setting {
    Setting {
        key: key.to_owned(),
        value: value.map(str::to_owned),
        source,
    }
}

fn read_config(load: &Result<Snapshot, String>) -> App {
    let mut app = on(Screen::Config);
    assert!(config::backfill(&mut app, &|| load.clone()));
    app
}

#[test]
fn snapshots_configuration_and_doctor_empty() {
    let app = read_config(&Ok(Snapshot {
        settings: Vec::new(),
        checks: Vec::new(),
    }));
    check(
        Screen::Config,
        State::Empty,
        &app,
        &["Configuration and doctor"],
    );
}

#[test]
fn snapshots_configuration_and_doctor_loading() {
    check(
        Screen::Config,
        State::Loading,
        &on(Screen::Config),
        &["Reading the configuration and running the checks…"],
    );
}

#[test]
fn snapshots_configuration_and_doctor_populated() {
    let app = read_config(&Ok(Snapshot {
        settings: vec![
            setting("provider", Some("\"claude\""), Source::GlobalFile),
            setting("model", Some("\"opus\""), Source::Env),
            setting("retention_days", Some("45"), Source::ProjectFile),
            setting(
                "verify_command",
                Some("[\"make\", \"test\"]"),
                Source::ProjectFile,
            ),
            setting("output_ring_lines", Some("4096"), Source::Default),
            setting("push_remote", None, Source::Default),
        ],
        checks: vec![
            CheckResult {
                check: "git",
                status: CheckStatus::Pass,
                detail: "git 2.51.0".to_owned(),
                remedy: None,
            },
            CheckResult {
                check: "provider",
                status: CheckStatus::Pass,
                detail: "claude found on PATH".to_owned(),
                remedy: None,
            },
        ],
    }));
    check(
        Screen::Config,
        State::Populated,
        &app,
        &["retention_days", "project file", "git 2.51.0"],
    );
}

#[test]
fn snapshots_configuration_and_doctor_error() {
    let failed = read_config(&Err(
        "cannot read .ktask/config.toml: unknown key `retention`".to_owned(),
    ));
    assert!(failed.config.failure().is_some());
    check(
        Screen::Config,
        State::Error,
        &failed,
        &["unknown key `retention`"],
    );
}

// ---- coverage ----

/// Where the committed snapshots are.
fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
}

#[test]
fn snapshots_every_screen_has_a_snapshot_in_every_state() {
    let missing: Vec<String> = Screen::ALL
        .into_iter()
        .flat_map(|screen| State::ALL.map(|state| snapshot_name(screen, state)))
        .filter(|name| {
            !snapshot_dir()
                .join(format!("snapshots__{name}.snap"))
                .is_file()
        })
        .collect();
    assert!(
        missing.is_empty(),
        "no committed snapshot for: {missing:?}; write a `snapshots_<screen>_<state>` test \
         that calls `check` and commit the snapshot it records"
    );
}

#[test]
fn snapshots_no_snapshot_is_left_without_a_screen_and_state() {
    let known: Vec<String> = Screen::ALL
        .into_iter()
        .flat_map(|screen| State::ALL.map(|state| snapshot_name(screen, state)))
        .collect();
    let stale: Vec<String> = std::fs::read_dir(snapshot_dir())
        .expect("snapshot directory")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|file| {
            file.strip_prefix("snapshots__")?
                .strip_suffix(".snap")
                .map(str::to_owned)
        })
        .filter(|name| !known.contains(name))
        .collect();
    assert!(stale.is_empty(), "snapshots for nothing: {stale:?}");
}

/// What the committed snapshot of `screen` in `state` holds.
fn recorded(screen: Screen, state: State) -> Fallible<String> {
    let name = snapshot_name(screen, state);
    Ok(std::fs::read_to_string(
        snapshot_dir().join(format!("snapshots__{name}.snap")),
    )?)
}

/// A snapshot that is the same in every state proves nothing about any of
/// them: it would go on passing if a screen stopped telling its states apart.
/// Empty, populated and error are always told apart; loading may look like
/// empty on the two screens with nothing to wait for.
#[test]
fn snapshots_a_screen_tells_its_states_apart() -> Fallible {
    for screen in Screen::ALL {
        let empty = recorded(screen, State::Empty)?;
        let loading = recorded(screen, State::Loading)?;
        let populated = recorded(screen, State::Populated)?;
        let error = recorded(screen, State::Error)?;
        assert_ne!(empty, populated, "{screen:?}: empty looks like populated");
        assert_ne!(empty, error, "{screen:?}: empty looks like error");
        assert_ne!(populated, error, "{screen:?}: populated looks like error");
        let waits_for_nothing = matches!(screen, Screen::Failures | Screen::InputInbox);
        if !waits_for_nothing {
            assert_ne!(empty, loading, "{screen:?}: empty looks like loading");
        }
        assert_ne!(
            loading, populated,
            "{screen:?}: loading looks like populated"
        );
    }
    Ok(())
}

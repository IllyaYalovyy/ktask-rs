//! A property test over arbitrary event sequences: whatever the interface is
//! sent, in whatever order, at whatever terminal size, [`update`] and
//! [`render`] return, and they do not panic (docs/CONTRACT.md §5).
//!
//! The events are everything an [`AppEvent`] can be:
//!
//! - **keys**: every entry of [`BINDINGS`] with its modifiers, keys the
//!   contract does not bind (Enter, Backspace, arrows, page keys, any
//!   character), and any combination of modifiers and key kind;
//! - **journal events**: every [`EventKind`] variant, on a few task ids so that
//!   they land on the same task, with hostile text (escape sequences, wide and
//!   combining characters, control characters, very long lines) and extreme
//!   numbers;
//! - **resizes**, including a terminal with no columns or no rows;
//! - **ticks**.
//!
//! A case drives a [`Harness`], which runs the real `update` and the real
//! `render` into a test backend after every event. It runs on its own thread
//! under a deadline, so a panic and a hang are both a failed case that
//! proptest then shrinks to the shortest sequence that still fails, instead of
//! a panic that tears down the test or a run that never ends.
//!
//! The strategy and the runner are themselves tested (`property_*` below):
//! a generator that never produced a `Resize(0, 0)`, or a runner that
//! reported a panic as a pass, would leave the property test green and
//! worthless.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ktask_core::{
    AttemptId, AttemptRecord, DecisionRequest, Event, EventKind, EventSeq, FailureClass, GateKind,
    GateResult, PauseReason, Phase, Recovery, Stream, TaskId, TddException, Usage, UsageSource,
};
use ktask_tui::testing::Harness;
use ktask_tui::{AppEvent, BINDINGS, KeyAction, Screen, lookup};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, FileFailurePersistence, TestCaseError, TestError, TestRunner};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use time::OffsetDateTime;

/// How many sequences the property test runs.
const CASES: u32 = 512;

/// The longest a sequence may take. A sequence of a few dozen events is drawn
/// in milliseconds; this is generous enough that a loaded machine does not
/// fail a case, and short enough that a hang fails the test in well under a
/// minute.
const DEADLINE: Duration = Duration::from_secs(30);

/// The longest sequence generated.
const MAX_EVENTS: usize = 48;

/// How many variants [`EventKind`] has. [`ordinal`] is an exhaustive match, so
/// adding a variant stops this file compiling until the generator learns it.
const VARIANTS: usize = 29;

// ---- generating events ----

/// A time-of-day source for events: the epoch, a spread of ordinary instants,
/// and the ends of what [`OffsetDateTime`] can hold.
fn arb_time() -> impl Strategy<Value = OffsetDateTime> {
    prop_oneof![
        4 => (0_i64..4_000_000_000).prop_map(|secs| {
            OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH)
        }),
        1 => Just(OffsetDateTime::UNIX_EPOCH),
        1 => Just(time::PrimitiveDateTime::MIN.assume_utc()),
        1 => Just(time::PrimitiveDateTime::MAX.assume_utc()),
    ]
}

/// Text as the journal might hold it, and as it should never be trusted to be:
/// anything at all, or a piece chosen to break a layout.
fn arb_text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => "\\PC{0,40}",
        2 => any::<String>(),
        1 => Just(String::new()),
        1 => Just("\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}".to_string()),
        1 => Just("日本語のテキスト 🙂 ｆｕｌｌｗｉｄｔｈ".to_string()),
        1 => Just("e\u{301}\u{301}\u{200d}\u{200b}\u{feff}x".to_string()),
        1 => Just("one\r\ntwo\rthree\nfour\ttab\u{0}nul".to_string()),
        1 => "[a-z ]{200,600}",
        1 => Just("a\u{fffd}b\u{85}c\u{2028}d".to_string()),
    ]
}

/// A task id on one of a few tasks, so that events pile up on the same ones.
fn arb_task() -> impl Strategy<Value = Option<TaskId>> {
    prop_oneof![
        8 => (0_u32..4).prop_map(|n| Some(TaskId::new(n))),
        1 => Just(None),
        1 => Just(Some(TaskId::new(u32::MAX))),
    ]
}

fn arb_attempt() -> impl Strategy<Value = AttemptId> {
    prop_oneof![
        8 => (0_u32..4).prop_map(AttemptId::new),
        1 => Just(AttemptId::new(u32::MAX)),
    ]
}

fn arb_class() -> impl Strategy<Value = FailureClass> {
    prop::sample::select(vec![
        FailureClass::AgentFailure,
        FailureClass::VerificationFailure,
        FailureClass::ProviderLimit,
        FailureClass::ProviderTransient,
        FailureClass::ProviderConfiguration,
        FailureClass::GitConflict,
        FailureClass::EnvironmentFailure,
        FailureClass::PolicyFailure,
        FailureClass::NeedsInput,
    ])
}

fn arb_phase() -> impl Strategy<Value = Phase> {
    prop::sample::select(vec![
        Phase::Goal,
        Phase::Scope,
        Phase::AcceptanceTests,
        Phase::Implement,
        Phase::Red,
        Phase::Green,
        Phase::Refactor,
        Phase::Review,
        Phase::Harden,
        Phase::DoneCheck,
        Phase::Verify,
        Phase::Publish,
    ])
}

fn arb_gate_kind() -> impl Strategy<Value = GateKind> {
    prop::sample::select(vec![
        GateKind::Baseline,
        GateKind::Targeted,
        GateKind::Verify,
        GateKind::Lint,
        GateKind::Format,
        GateKind::Build,
        GateKind::Privacy,
    ])
}

fn arb_stream() -> impl Strategy<Value = Stream> {
    prop::sample::select(vec![Stream::Stdout, Stream::Stderr])
}

fn arb_gate_result() -> impl Strategy<Value = GateResult> {
    (
        arb_gate_kind(),
        any::<bool>(),
        any::<Option<i32>>(),
        any::<Option<i32>>(),
        any::<u64>(),
        (arb_text(), arb_text(), any::<bool>()),
    )
        .prop_map(
            |(kind, passed, exit_code, signal, duration_ms, (stdout, stderr, timed_out))| {
                GateResult {
                    kind,
                    passed,
                    exit_code,
                    signal,
                    duration_ms,
                    stdout,
                    stderr,
                    timed_out,
                }
            },
        )
}

fn arb_usage() -> impl Strategy<Value = Usage> {
    (
        any::<Option<u64>>(),
        any::<Option<u64>>(),
        any::<Option<u64>>(),
        prop::option::of(prop_oneof![
            Just(0.0),
            Just(f64::NAN),
            Just(f64::INFINITY),
            Just(f64::NEG_INFINITY),
            Just(-1.5),
            any::<f64>(),
        ]),
        prop::sample::select(vec![
            UsageSource::Provider,
            UsageSource::ParsedFromOutput,
            UsageSource::Unavailable,
        ]),
    )
        .prop_map(
            |(input_tokens, output_tokens, cached_tokens, cost_usd, source)| Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
                cost_usd,
                source,
            },
        )
}

fn arb_decision() -> impl Strategy<Value = DecisionRequest> {
    (
        arb_text(),
        prop::collection::vec(arb_text(), 0..4),
        arb_text(),
        arb_text(),
        prop::option::of(arb_text()),
    )
        .prop_map(
            |(question, options, tradeoffs, impact, recommended)| DecisionRequest {
                question,
                options,
                tradeoffs,
                impact,
                recommended,
            },
        )
}

fn arb_record() -> impl Strategy<Value = AttemptRecord> {
    (
        (
            arb_attempt(),
            arb_task(),
            arb_time(),
            prop::option::of(arb_time()),
        ),
        (
            prop::option::of(arb_text()),
            prop::option::of(arb_text()),
            prop::option::of(arb_text()),
            arb_text(),
        ),
        (
            prop::collection::vec(arb_gate_result(), 0..3),
            prop::option::of(arb_usage()),
            arb_text(),
            prop::option::of(arb_text()),
        ),
    )
        .prop_map(
            |(
                (id, task, started, ended),
                (model_configured, model_reported, session_id, exit_reason),
                (gates, usage, base_sha, candidate_sha),
            )| AttemptRecord {
                id,
                task: task.unwrap_or(TaskId::new(0)),
                started,
                ended,
                model_configured,
                model_reported,
                session_id,
                exit_reason,
                gates,
                usage,
                base_sha,
                candidate_sha,
            },
        )
}

fn arb_pause() -> impl Strategy<Value = PauseReason> {
    prop_oneof![
        prop::option::of(arb_time()).prop_map(|until| PauseReason::Limit { until }),
        Just(PauseReason::Input),
        Just(PauseReason::HumanGate),
        Just(PauseReason::Interrupted),
        Just(PauseReason::Blocked),
    ]
}

/// Every [`EventKind`] variant, in the order of [`ordinal`]. It is built in
/// two halves only because one `prop_oneof!` takes a bounded number of arms.
fn arb_kind() -> impl Strategy<Value = EventKind> {
    prop_oneof![arb_kind_early(), arb_kind_late()]
}

fn arb_kind_early() -> impl Strategy<Value = EventKind> {
    prop_oneof![
        arb_text().prop_map(|title| EventKind::TaskQueued { title }),
        Just(EventKind::PreflightStarted),
        arb_text().prop_map(|base_sha| EventKind::PreflightPassed { base_sha }),
        (arb_class(), arb_text())
            .prop_map(|(class, detail)| EventKind::PreflightFailed { class, detail }),
        (arb_attempt(), arb_text(), any::<u32>(), arb_text()).prop_map(
            |(attempt, protocol, pid, base_sha)| EventKind::AttemptStarted {
                attempt,
                protocol,
                pid,
                base_sha,
            }
        ),
        (arb_attempt(), arb_phase())
            .prop_map(|(attempt, phase)| EventKind::PhaseEntered { attempt, phase }),
        (arb_attempt(), arb_stream(), arb_text()).prop_map(|(attempt, stream, text)| {
            EventKind::AgentOutput {
                attempt,
                stream,
                text,
            }
        }),
        (
            arb_attempt(),
            any::<i32>(),
            prop::option::of(arb_usage()),
            prop::option::of(arb_text()),
            prop::option::of(arb_text()),
        )
            .prop_map(|(attempt, exit_code, usage, session_id, model_reported)| {
                EventKind::AttemptFinished {
                    attempt,
                    exit_code,
                    usage,
                    session_id,
                    model_reported,
                }
            }),
        arb_gate_kind().prop_map(|gate| EventKind::GateStarted { gate }),
        arb_gate_result().prop_map(|result| EventKind::GateFinished { result }),
        arb_gate_result().prop_map(|result| EventKind::GateRerun { result }),
        arb_attempt().prop_map(|attempt| EventKind::VerifyPassed { attempt }),
        (arb_attempt(), arb_class(), arb_text()).prop_map(|(attempt, class, detail)| {
            EventKind::VerifyFailed {
                attempt,
                class,
                detail,
            }
        }),
        (arb_attempt(), arb_text()).prop_map(|(attempt, candidate_sha)| {
            EventKind::PublishStarted {
                attempt,
                candidate_sha,
            }
        }),
        (arb_text(), arb_text())
            .prop_map(|(commit, remote_sha)| EventKind::PublishVerified { commit, remote_sha }),
    ]
}

fn arb_kind_late() -> impl Strategy<Value = EventKind> {
    prop_oneof![
        arb_text().prop_map(|commit| EventKind::TaskDone { commit }),
        (arb_class(), arb_text())
            .prop_map(|(class, detail)| EventKind::TaskFailed { class, detail }),
        arb_attempt().prop_map(|attempt| EventKind::RetryStarted { attempt }),
        arb_text().prop_map(|reason| EventKind::TaskCancelled { reason }),
        arb_pause().prop_map(|reason| EventKind::Paused { reason }),
        Just(EventKind::Resumed),
        arb_phase().prop_map(|phase| EventKind::Interrupted { phase }),
        (
            prop::sample::select(vec![
                Recovery::Resume,
                Recovery::MarkInterrupted,
                Recovery::AlreadyApplied,
            ]),
            arb_text(),
        )
            .prop_map(|(decision, detail)| EventKind::RecoveryDecision { decision, detail }),
        (
            prop::sample::select(vec![
                TddException::Documentation,
                TddException::PureRefactoring,
                TddException::BuildConfiguration,
                TddException::PreExistingFailingTest,
            ]),
            arb_text(),
        )
            .prop_map(|(exception, reason)| EventKind::TddExceptionUsed { exception, reason }),
        arb_decision().prop_map(|request| EventKind::DecisionRaised { request }),
        (arb_text(), arb_text()).prop_map(|(path, answer)| EventKind::DecisionResolved {
            adr_path: PathBuf::from(path),
            answer,
        }),
        (arb_text(), arb_time()).prop_map(|(by, at)| EventKind::GateAcknowledged { by, at }),
        arb_record().prop_map(|record| EventKind::AttemptRecorded {
            record: Box::new(record),
        }),
        (
            arb_attempt(),
            arb_class(),
            prop::collection::vec(arb_text(), 0..4),
            arb_text(),
        )
            .prop_map(
                |(attempt, class, repairs, outcome)| EventKind::SelfHealingReport {
                    attempt,
                    class,
                    repairs,
                    outcome,
                }
            ),
    ]
}

/// Which variant `kind` is, as a number below [`VARIANTS`].
///
/// Exhaustive on purpose: a new [`EventKind`] variant does not compile here
/// until it has a number, and [`property_the_generator_makes_every_journal_event_variant`]
/// then fails until [`arb_kind`] makes it.
fn ordinal(kind: &EventKind) -> usize {
    match kind {
        EventKind::TaskQueued { .. } => 0,
        EventKind::PreflightStarted => 1,
        EventKind::PreflightPassed { .. } => 2,
        EventKind::PreflightFailed { .. } => 3,
        EventKind::AttemptStarted { .. } => 4,
        EventKind::PhaseEntered { .. } => 5,
        EventKind::AgentOutput { .. } => 6,
        EventKind::AttemptFinished { .. } => 7,
        EventKind::GateStarted { .. } => 8,
        EventKind::GateFinished { .. } => 9,
        EventKind::GateRerun { .. } => 10,
        EventKind::VerifyPassed { .. } => 11,
        EventKind::VerifyFailed { .. } => 12,
        EventKind::PublishStarted { .. } => 13,
        EventKind::PublishVerified { .. } => 14,
        EventKind::TaskDone { .. } => 15,
        EventKind::TaskFailed { .. } => 16,
        EventKind::RetryStarted { .. } => 17,
        EventKind::TaskCancelled { .. } => 18,
        EventKind::Paused { .. } => 19,
        EventKind::Resumed => 20,
        EventKind::Interrupted { .. } => 21,
        EventKind::RecoveryDecision { .. } => 22,
        EventKind::TddExceptionUsed { .. } => 23,
        EventKind::DecisionRaised { .. } => 24,
        EventKind::DecisionResolved { .. } => 25,
        EventKind::GateAcknowledged { .. } => 26,
        EventKind::AttemptRecorded { .. } => 27,
        EventKind::SelfHealingReport { .. } => 28,
    }
}

fn arb_journal_event() -> impl Strategy<Value = Event> {
    (
        prop_oneof![
            8 => 0_u64..64,
            1 => Just(0),
            1 => Just(u64::MAX),
        ],
        arb_time(),
        arb_task(),
        arb_kind(),
    )
        .prop_map(|(seq, ts, task_id, kind)| Event {
            seq: EventSeq::new(seq),
            ts,
            task_id,
            kind,
        })
}

/// The presses the contract binds, exactly as the table spells them.
fn binding_presses() -> Vec<KeyEvent> {
    BINDINGS
        .iter()
        .map(|binding| KeyEvent::new(binding.key, binding.modifiers))
        .collect()
}

fn arb_key_code() -> impl Strategy<Value = KeyCode> {
    prop_oneof![
        8 => any::<char>().prop_map(KeyCode::Char),
        6 => prop::sample::select(vec![
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Null,
        ]),
        1 => any::<u8>().prop_map(KeyCode::F),
    ]
}

/// Any key at all: any code, any modifiers, any kind of press.
fn arb_any_key() -> impl Strategy<Value = KeyEvent> {
    (
        arb_key_code(),
        any::<u8>(),
        prop::sample::select(vec![
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ]),
    )
        .prop_map(|(code, modifiers, kind)| KeyEvent {
            code,
            modifiers: KeyModifiers::from_bits_truncate(modifiers),
            kind,
            state: KeyEventState::NONE,
        })
}

fn arb_key() -> impl Strategy<Value = KeyEvent> {
    prop_oneof![
        3 => prop::sample::select(binding_presses()),
        1 => arb_any_key(),
    ]
}

/// A terminal size: mostly one an operator has, and often one they cannot,
/// with no columns, no rows, or neither.
fn arb_size() -> impl Strategy<Value = (u16, u16)> {
    prop_oneof![
        3 => (0_u16..=4, 0_u16..=4),
        2 => (0_u16..=4, 0_u16..=60),
        2 => (0_u16..=200, 0_u16..=4),
        4 => (5_u16..=200, 5_u16..=60),
        1 => Just((80, 24)),
        1 => Just((79, 23)),
    ]
}

fn arb_event() -> impl Strategy<Value = AppEvent> {
    prop_oneof![
        5 => arb_key().prop_map(AppEvent::Key),
        5 => arb_journal_event().prop_map(AppEvent::Core),
        2 => arb_size().prop_map(|(w, h)| AppEvent::Resize(w, h)),
        1 => Just(AppEvent::Tick),
    ]
}

/// A terminal to start on and the events to send it.
fn arb_run() -> impl Strategy<Value = Run> {
    (
        arb_size(),
        prop::collection::vec(arb_event(), 0..=MAX_EVENTS),
    )
        .prop_map(|(size, events)| Run { size, events })
}

/// One case: the size the terminal starts at and what happens to it.
#[derive(Debug, Clone)]
struct Run {
    size: (u16, u16),
    events: Vec<AppEvent>,
}

// ---- running a case under a deadline ----

/// Why a case failed.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Panicked(String),
    TimedOut,
}

/// Runs `work` on its own thread and waits at most `deadline` for it.
///
/// A panic comes back as [`Failure::Panicked`] with its message, and a run
/// that does not finish as [`Failure::TimedOut`]; the runaway thread is left
/// behind, which is fine for a test that is about to fail.
fn bounded<T: Send + 'static>(
    deadline: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Failure> {
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        // The receiver is gone only when the deadline already passed.
        let _ = tx.send(work());
    });
    match rx.recv_timeout(deadline) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(Failure::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let message = match worker.join() {
                Ok(()) => "the worker ended without a result".to_string(),
                Err(payload) => payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(ToString::to_string))
                    .unwrap_or_else(|| "a panic with a payload that is not text".to_string()),
            };
            Err(Failure::Panicked(message))
        }
    }
}

/// What a finished run left on the screen.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    size: (u16, u16),
    drawn: (u16, u16),
    text: String,
}

/// Plays `run` through a [`Harness`], which redraws after every event.
fn play(run: Run) -> Outcome {
    let mut harness = Harness::new(run.size.0, run.size.1);
    for event in run.events {
        harness.send(event);
    }
    let area = harness.buffer().area;
    Outcome {
        size: harness.app().size,
        drawn: (area.width, area.height),
        text: harness.text(),
    }
}

/// The property: a run finishes without a panic, draws a screen exactly as
/// large as the terminal, and draws the same screen when played again.
fn holds(run: &Run) -> Result<(), String> {
    let first = bounded(DEADLINE, {
        let run = run.clone();
        move || play(run)
    })
    .map_err(|failure| format!("{failure:?}"))?;
    if first.drawn != first.size {
        return Err(format!(
            "drew {:?} for a terminal of {:?}",
            first.drawn, first.size
        ));
    }
    let again = bounded(DEADLINE, {
        let run = run.clone();
        move || play(run)
    })
    .map_err(|failure| format!("on the second play: {failure:?}"))?;
    if again != first {
        return Err("the same events drew two different screens".to_string());
    }
    Ok(())
}

// ---- the property ----

static EXECUTED: AtomicUsize = AtomicUsize::new(0);

#[test]
fn property_arbitrary_event_sequences_never_panic_and_always_terminate() {
    let mut runner = TestRunner::new(Config {
        cases: CASES,
        // A failing seed is kept here and replayed first on every later run.
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/property.proptest-regressions",
        ))),
        ..Config::default()
    });
    let result = runner.run(&arb_run(), |run| {
        EXECUTED.fetch_add(1, Ordering::Relaxed);
        holds(&run).map_err(TestCaseError::fail)
    });
    if let Err(error) = result {
        panic!("{error}");
    }
    assert_eq!(EXECUTED.load(Ordering::Relaxed), CASES as usize);
}

// ---- the property test's own tests ----

#[test]
fn property_test_runs_at_least_512_cases() {
    // The contract's floor, against the constant the property test uses.
    assert_eq!(CASES, 512);
}

#[test]
fn property_bounded_returns_the_value_of_work_that_finishes() {
    assert_eq!(bounded(Duration::from_secs(30), || 6 * 7), Ok(42));
}

#[test]
fn property_bounded_reports_a_panic_with_its_message() {
    let failure = bounded(Duration::from_secs(30), || -> u8 {
        panic!("out of range: {}", 9);
    });
    assert_eq!(
        failure,
        Err(Failure::Panicked("out of range: 9".to_string()))
    );
    let failure = bounded(Duration::from_secs(30), || -> u8 {
        panic!("a fixed message");
    });
    assert_eq!(
        failure,
        Err(Failure::Panicked("a fixed message".to_string()))
    );
}

#[test]
fn property_bounded_reports_work_that_does_not_finish_as_timed_out() {
    let (release, hold) = mpsc::channel::<()>();
    let failure = bounded(Duration::from_millis(50), move || {
        // Blocks until the test lets go of the sender.
        let _ = hold.recv();
    });
    assert_eq!(failure, Err(Failure::TimedOut));
    drop(release);
}

#[test]
fn property_holds_accepts_an_ordinary_run_and_a_degenerate_one() {
    let ordinary = Run {
        size: (80, 24),
        events: vec![
            AppEvent::Tick,
            AppEvent::Resize(0, 0),
            AppEvent::Resize(9, 3),
        ],
    };
    assert_eq!(holds(&ordinary), Ok(()));
    let empty = Run {
        size: (0, 0),
        events: Vec::new(),
    };
    assert_eq!(holds(&empty), Ok(()));
}

#[test]
fn property_play_reports_the_size_the_terminal_ended_at_and_what_was_drawn() {
    let outcome = play(Run {
        size: (20, 5),
        events: vec![AppEvent::Resize(7, 2)],
    });
    assert_eq!(outcome.size, (7, 2));
    assert_eq!(outcome.drawn, (7, 2));
    assert_eq!(outcome.text.split('\n').count(), 2);
    assert!(outcome.text.starts_with("1 Queue"));
}

/// What a helper that can fail returns, so that the failure reaches the test
/// that asked and is not a panic in a helper.
type Fallible<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Draws `count` values from `strategy`.
fn sample<S: Strategy>(strategy: &S, count: usize) -> Fallible<Vec<S::Value>> {
    let mut runner = TestRunner::deterministic();
    (0..count)
        .map(|_| {
            let tree = strategy
                .new_tree(&mut runner)
                .map_err(|reason| format!("the strategy made no value: {reason}"))?;
            Ok(tree.current())
        })
        .collect()
}

#[test]
fn property_the_generator_makes_every_journal_event_variant() -> Fallible {
    let seen: BTreeSet<usize> = sample(&arb_kind(), 4_000)?.iter().map(ordinal).collect();
    assert_eq!(seen, (0..VARIANTS).collect::<BTreeSet<_>>());
    Ok(())
}

#[test]
fn property_the_generator_makes_every_key_in_the_binding_table() -> Fallible {
    let presses = binding_presses();
    assert_eq!(presses.len(), BINDINGS.len());
    let seen: BTreeSet<String> = sample(&arb_key(), 4_000)?
        .iter()
        .map(|key| format!("{key:?}"))
        .collect();
    for press in &presses {
        assert!(
            seen.contains(&format!("{press:?}")),
            "the generator never pressed {press:?}"
        );
    }
    Ok(())
}

#[test]
fn property_every_key_the_generator_takes_from_the_table_resolves_to_its_binding() {
    // A press that the lookup does not recognise would exercise nothing.
    for (binding, press) in BINDINGS.iter().zip(binding_presses()) {
        for screen in binding.screens {
            assert_eq!(
                lookup(*screen, &press).map(|found| found.action),
                Some(binding.action),
                "{press:?} on {screen:?}"
            );
        }
    }
}

#[test]
fn property_the_generator_makes_degenerate_and_ordinary_terminals() -> Fallible {
    let sizes = sample(&arb_size(), 4_000)?;
    assert!(sizes.contains(&(0, 0)));
    assert!(sizes.iter().any(|(w, h)| *w == 0 && *h > 4));
    assert!(sizes.iter().any(|(w, h)| *h == 0 && *w > 4));
    assert!(sizes.iter().any(|(w, h)| *w == 1 && *h == 1));
    assert!(sizes.contains(&(80, 24)));
    assert!(sizes.iter().any(|(w, h)| *w >= 100 && *h >= 30));
    Ok(())
}

#[test]
fn property_the_generator_makes_all_four_kinds_of_event() -> Fallible {
    let events = sample(&arb_event(), 1_000)?;
    assert!(events.iter().any(|e| matches!(e, AppEvent::Key(_))));
    assert!(events.iter().any(|e| matches!(e, AppEvent::Core(_))));
    assert!(events.iter().any(|e| matches!(e, AppEvent::Resize(..))));
    assert!(events.iter().any(|e| matches!(e, AppEvent::Tick)));
    Ok(())
}

/// The press that shows `screen`, read from the table.
fn jump_to(screen: Screen) -> Fallible<KeyEvent> {
    let binding = BINDINGS
        .iter()
        .find(|binding| binding.action == KeyAction::Jump(screen))
        .ok_or_else(|| format!("{screen:?} has no number key"))?;
    Ok(KeyEvent::new(binding.key, binding.modifiers))
}

#[test]
fn property_every_binding_on_every_screen_terminates_on_every_terminal_size() -> Fallible {
    // The random sequences reach the bindings by chance; this reaches each of
    // them from each screen, on a terminal of each kind, on purpose.
    for &(w, h) in &[(0, 0), (1, 1), (0, 24), (80, 0), (30, 10), (80, 24)] {
        for screen in Screen::ALL {
            for press in binding_presses() {
                let events = vec![AppEvent::Key(jump_to(screen)?), AppEvent::Key(press)];
                let run = Run {
                    size: (w, h),
                    events,
                };
                assert_eq!(holds(&run), Ok(()), "{screen:?} at {w}x{h}: {press:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn property_a_failing_sequence_shrinks_to_the_events_that_matter() {
    // A stand-in property that fails for a resize to nothing, checked against
    // what the runner reports: the shortest sequence that still fails.
    let mut runner = TestRunner::new(Config {
        cases: CASES,
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&arb_run(), |run| {
        let resized_to_nothing = run
            .events
            .iter()
            .any(|event| matches!(event, AppEvent::Resize(0, 0)));
        prop_assert!(!resized_to_nothing);
        Ok(())
    });
    let Err(TestError::Fail(_, run)) = result else {
        panic!("a sequence with Resize(0, 0) in it was never generated: {result:?}");
    };
    assert_eq!(run.events, vec![AppEvent::Resize(0, 0)]);
}

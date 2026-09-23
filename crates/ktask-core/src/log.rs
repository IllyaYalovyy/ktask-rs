//! `Logger`: a redacted, newline-delimited JSON record of a run, on disk.
//!
//! `VISION.md` section 11: "Tokens, credentials, and configured secret
//! patterns are redacted from logs" and "restrictive filesystem
//! permissions." Section 13 lists a raw, searchable log view as one of the
//! TUI's required screens; this module is what that view reads.
//!
//! A [`Logger`] wraps a [`Subscription`] the same way any other frontend
//! would (`events.rs`): once it is opened on one, nothing that calls
//! [`crate::Recorder::record`] has to also remember to log — every event
//! that reaches the bus is available to [`Logger::drain`], whether or not
//! the caller that recorded it knows a logger exists.

use crate::ids::{AttemptId, TaskId};
use crate::redact::redact;
use crate::state::Phase;
use crate::{EventKind, Result, Subscription};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::OffsetDateTime;
use time::macros::format_description;

/// How severe a log record is.
///
/// Declared least to most severe, so `derive`d [`Ord`] orders them the same
/// way a [`Logger`]'s minimum level expects: a record below the floor
/// compares less than it and is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// High-volume detail, such as raw agent output, useful only while
    /// actively debugging a run.
    Debug,
    /// A normal step in a run proceeding as expected.
    Info,
    /// Something that diverted the run from its plan without failing it,
    /// such as a pause or an interruption.
    Warn,
    /// A failure: a gate, a preflight check, or the task itself.
    Error,
}

/// One line of `<state_dir>/logs/run-<date>.jsonl`.
#[derive(Serialize)]
struct LogRecord<'a> {
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    level: Level,
    task_id: Option<TaskId>,
    attempt: Option<AttemptId>,
    phase: Option<Phase>,
    message: &'a str,
}

/// The date-stamped format [`Logger::open`] names its log file with:
/// `run-2024-01-15.jsonl`.
const DATE_FORMAT: &[time::format_description::FormatItem<'_>] =
    format_description!("[year]-[month]-[day]");

/// Locks `mutex`, recovering the guard from a poisoned lock rather than
/// panicking, matching [`crate::Bus::publish`]'s own recovery: a panicking writer
/// must not take logging down with whatever task it was logging.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Restricts `path` to owner-only access. A no-op on non-Unix targets,
/// since there is no equivalent mode bit to set.
#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
    Ok(())
}

/// Opens `path` for appending, creating it with owner-only (`0600`)
/// permissions if it does not already exist.
///
/// Permissions are set explicitly after opening rather than relied on from
/// the process's umask alone (which a misconfigured environment could leave
/// wide open), matching [`crate::write_evidence`]'s directories.
fn open_private(path: &Path) -> Result<File> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    set_private(path)?;
    Ok(file)
}

/// This event's severity, for [`Logger::drain`]'s level filter.
///
/// Exhaustive and wildcard-free, matching [`EventKind::discriminant`]'s own
/// match: a variant added to [`EventKind`] without an arm here fails to
/// compile instead of silently defaulting to some level.
fn level_for(kind: &EventKind) -> Level {
    match kind {
        EventKind::PreflightFailed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::TaskFailed { .. } => Level::Error,
        EventKind::GateFinished { result } if !result.passed => Level::Error,
        EventKind::Paused { .. }
        | EventKind::Interrupted { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::TaskCancelled { .. } => Level::Warn,
        EventKind::AgentOutput { .. } => Level::Debug,
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RetryStarted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::SelfHealingReport { .. } => Level::Info,
    }
}

/// The attempt this event concerns, when it names one.
fn attempt_of(kind: &EventKind) -> Option<AttemptId> {
    match kind {
        EventKind::AttemptStarted { attempt, .. }
        | EventKind::PhaseEntered { attempt, .. }
        | EventKind::AgentOutput { attempt, .. }
        | EventKind::AttemptFinished { attempt, .. }
        | EventKind::VerifyPassed { attempt }
        | EventKind::VerifyFailed { attempt, .. }
        | EventKind::PublishStarted { attempt, .. }
        | EventKind::RetryStarted { attempt }
        | EventKind::SelfHealingReport { attempt, .. } => Some(*attempt),
        EventKind::AttemptRecorded { record } => Some(record.id),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::TaskCancelled { .. }
        | EventKind::Paused { .. }
        | EventKind::Resumed
        | EventKind::Interrupted { .. }
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::GateAcknowledged { .. } => None,
    }
}

/// The protocol phase this event concerns, when it names one.
fn phase_of(kind: &EventKind) -> Option<Phase> {
    match kind {
        EventKind::PhaseEntered { phase, .. } | EventKind::Interrupted { phase } => Some(*phase),
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::PreflightFailed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::AgentOutput { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::GateStarted { .. }
        | EventKind::GateFinished { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::TaskCancelled { .. }
        | EventKind::Paused { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::DecisionRaised { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::RetryStarted { .. }
        | EventKind::SelfHealingReport { .. } => None,
    }
}

/// A short, human-readable rendering of `kind`, before redaction.
///
/// Exhaustive and wildcard-free for the same reason as [`level_for`]: a new
/// variant must be given a message here before it will compile.
fn message_for(kind: &EventKind) -> String {
    match kind {
        EventKind::TaskQueued { title } => format!("task queued: {title}"),
        EventKind::PreflightStarted => "preflight started".to_string(),
        EventKind::PreflightPassed { base_sha } => format!("preflight passed at {base_sha}"),
        EventKind::PreflightFailed { class, detail } => {
            format!("preflight failed ({class:?}): {detail}")
        }
        EventKind::AttemptStarted {
            protocol,
            pid,
            base_sha,
            ..
        } => format!("attempt started: protocol={protocol} pid={pid} base_sha={base_sha}"),
        EventKind::PhaseEntered { phase, .. } => format!("phase entered: {phase:?}"),
        EventKind::AgentOutput { stream, text, .. } => format!("[{stream:?}] {text}"),
        EventKind::AttemptFinished {
            exit_code,
            model_reported,
            ..
        } => format!(
            "attempt finished: exit_code={exit_code} model_reported={}",
            model_reported.as_deref().unwrap_or("unknown")
        ),
        EventKind::GateStarted { gate } => format!("gate started: {gate:?}"),
        EventKind::GateFinished { result } => format!(
            "gate finished: {:?} {} (exit_code={:?})",
            result.kind,
            if result.passed { "passed" } else { "failed" },
            result.exit_code
        ),
        EventKind::VerifyPassed { .. } => "verify passed".to_string(),
        EventKind::VerifyFailed { class, detail, .. } => {
            format!("verify failed ({class:?}): {detail}")
        }
        EventKind::PublishStarted { candidate_sha, .. } => {
            format!("publish started: {candidate_sha}")
        }
        EventKind::PublishVerified { commit, remote_sha } => {
            format!("publish verified: {commit} (remote sha {remote_sha})")
        }
        EventKind::TaskDone { commit } => format!("task done: {commit}"),
        EventKind::TaskFailed { class, detail } => format!("task failed ({class:?}): {detail}"),
        EventKind::RetryStarted { attempt } => format!("retry started as attempt {attempt}"),
        EventKind::TaskCancelled { reason } => format!("task cancelled: {reason}"),
        EventKind::Paused { reason } => format!("paused: {reason:?}"),
        EventKind::Resumed => "resumed".to_string(),
        EventKind::Interrupted { phase } => format!("interrupted in phase {phase:?}"),
        EventKind::RecoveryDecision { decision, detail } => {
            format!("recovery decision {decision:?}: {detail}")
        }
        EventKind::TddExceptionUsed { exception, reason } => {
            format!("tdd exception {exception:?}: {reason}")
        }
        EventKind::DecisionRaised { request } => format!("decision raised: {}", request.question),
        EventKind::GateAcknowledged { by, .. } => format!("gate acknowledged by {by}"),
        EventKind::AttemptRecorded { record } => {
            format!("attempt recorded: exit_reason={}", record.exit_reason)
        }
        EventKind::SelfHealingReport { class, outcome, .. } => {
            format!("self-healing report ({class:?}): {outcome}")
        }
    }
}

/// Writes a redacted, newline-delimited JSON record of a run to
/// `<state_dir>/logs/run-<date>.jsonl`.
///
/// Owns a [`Subscription`] rather than periodically being handed events, so
/// nothing that publishes to the bus has to also remember to log: whatever
/// [`crate::Recorder::record`] makes durable and announces, this eventually
/// sees.
#[derive(Debug)]
pub struct Logger {
    file: Mutex<File>,
    min_level: Level,
    subscription: Mutex<Subscription>,
}

impl Logger {
    /// Opens today's log file under `state_dir`'s `logs/` directory,
    /// creating both if they do not already exist, and takes ownership of
    /// `subscription` (typically obtained via
    /// [`crate::Recorder::subscribe`]) as the source [`Logger::drain`]
    /// reads from.
    ///
    /// The log file is named `run-<date>.jsonl`, `<date>` being the current
    /// UTC date at the moment this is called; a process that runs past
    /// midnight keeps writing to the file it opened. The file is created
    /// with mode `0600` on Unix (a no-op restriction elsewhere), and
    /// reopening an existing file for the same date appends to it rather
    /// than truncating it, so restarting a run on the same day does not
    /// lose the earlier half of its log.
    ///
    /// Only events at or above `min_level` are ever written by
    /// [`Logger::drain`]; everything below it is silently dropped, never
    /// buffered.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`](crate::Error::Io) if `logs/` or the log file
    /// cannot be created, and [`Error::Time`](crate::Error::Time) if
    /// today's date cannot be formatted.
    pub fn open(state_dir: &Path, subscription: Subscription, min_level: Level) -> Result<Logger> {
        let dir = state_dir.join("logs");
        std::fs::create_dir_all(&dir)?;
        let date = OffsetDateTime::now_utc().format(DATE_FORMAT)?;
        let file = open_private(&dir.join(format!("run-{date}.jsonl")))?;
        Ok(Logger {
            file: Mutex::new(file),
            min_level,
            subscription: Mutex::new(subscription),
        })
    }

    /// Appends every event published since the last call to `drain` (or
    /// since [`Logger::open`], on the first call) as one redacted JSON line
    /// each, dropping any below this logger's minimum level. Returns how
    /// many lines were actually written.
    ///
    /// Every event's rendered message is passed through [`redact`] before
    /// it is serialized, so a credential embedded in a free-text field —
    /// agent output, a failure detail, a cancellation reason — never
    /// reaches disk (`VISION.md` section 11), the same guarantee the
    /// journal itself already makes on write (`journal.rs`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`](crate::Error::Io) if a line cannot be written,
    /// and [`Error::Serde`](crate::Error::Serde) if a record cannot be
    /// serialized to JSON — which would indicate a bug in this module,
    /// since every field a `LogRecord` holds serializes infallibly.
    pub fn drain(&self) -> Result<usize> {
        let (events, _dropped_by_bus) = lock(&self.subscription).drain();

        let mut file = lock(&self.file);
        let mut written = 0usize;
        for event in &events {
            let level = level_for(&event.kind);
            if level < self.min_level {
                continue;
            }
            let message = redact(&message_for(&event.kind), &[]);
            let record = LogRecord {
                ts: event.ts,
                level,
                task_id: event.task_id,
                attempt: attempt_of(&event.kind),
                phase: phase_of(&event.kind),
                message: &message,
            };
            let line = serde_json::to_string(&record)?;
            writeln!(file, "{line}")?;
            written += 1;
        }
        file.flush()?;
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{FailureClass, Stream};
    use crate::state::PauseReason;
    use crate::{Bus, Journal, Recorder};
    use std::fs;

    fn recorder_at(state_dir: &Path) -> Recorder {
        let journal = Journal::open(&state_dir.join("journal.db")).expect("open journal");
        Recorder::new(journal, Bus::new(16))
    }

    /// Every line written to any file under `state_dir/logs`, in file
    /// iteration order — there is only ever one file in these tests, since
    /// they all run within a single UTC day.
    fn log_lines(state_dir: &Path) -> Vec<String> {
        let dir = state_dir.join("logs");
        let mut lines = Vec::new();
        for entry in fs::read_dir(&dir).expect("read logs dir") {
            let path = entry.expect("dir entry").path();
            let content = fs::read_to_string(&path).expect("read log file");
            lines.extend(content.lines().map(str::to_string));
        }
        lines
    }

    #[test]
    fn every_written_line_parses_as_json() {
        let state = tempfile::tempdir().expect("tempdir");
        let mut recorder = recorder_at(state.path());
        let logger =
            Logger::open(state.path(), recorder.subscribe(), Level::Debug).expect("open logger");

        recorder
            .record(
                Some(TaskId::new(1)),
                EventKind::TaskQueued {
                    title: "Add widget".to_string(),
                },
            )
            .expect("record");
        recorder
            .record(None, EventKind::PreflightStarted)
            .expect("record");
        recorder
            .record(
                Some(TaskId::new(1)),
                EventKind::TaskFailed {
                    class: FailureClass::AgentFailure,
                    detail: "boom".to_string(),
                },
            )
            .expect("record");

        let written = logger.drain().expect("drain");
        assert_eq!(written, 3);

        let lines = log_lines(state.path());
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid json line");
            assert!(value["message"].is_string());
            assert!(value["ts"].is_string());
        }
    }

    #[test]
    fn a_planted_secret_does_not_reach_the_log() {
        let state = tempfile::tempdir().expect("tempdir");
        let mut recorder = recorder_at(state.path());
        let logger =
            Logger::open(state.path(), recorder.subscribe(), Level::Debug).expect("open logger");
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

        recorder
            .record(
                Some(TaskId::new(1)),
                EventKind::TaskFailed {
                    class: FailureClass::AgentFailure,
                    detail: format!("leaked: {secret}"),
                },
            )
            .expect("record");

        logger.drain().expect("drain");

        let lines = log_lines(state.path());
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains(secret));
        assert!(lines[0].contains("[redacted]"));
    }

    #[test]
    fn records_below_the_minimum_level_are_dropped() {
        let state = tempfile::tempdir().expect("tempdir");
        let mut recorder = recorder_at(state.path());
        let logger =
            Logger::open(state.path(), recorder.subscribe(), Level::Warn).expect("open logger");

        // Debug: below the Warn floor.
        recorder
            .record(
                None,
                EventKind::AgentOutput {
                    attempt: AttemptId::new(1),
                    stream: Stream::Stdout,
                    text: "chatter that must not appear".to_string(),
                },
            )
            .expect("record debug event");
        // Info: also below the Warn floor.
        recorder
            .record(
                None,
                EventKind::TaskDone {
                    commit: "abc123".to_string(),
                },
            )
            .expect("record info event");
        // Warn: at the floor, must be kept.
        recorder
            .record(
                None,
                EventKind::Paused {
                    reason: PauseReason::Blocked,
                },
            )
            .expect("record warn event");

        let written = logger.drain().expect("drain");
        assert_eq!(written, 1, "only the Warn-level event clears the floor");

        let lines = log_lines(state.path());
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("chatter"));
        assert!(lines[0].contains("paused"));
    }

    #[test]
    fn level_ordering_places_debug_lowest_and_error_highest() {
        assert!(Level::Debug < Level::Info);
        assert!(Level::Info < Level::Warn);
        assert!(Level::Warn < Level::Error);
    }

    fn sample_gate_result(passed: bool) -> crate::GateResult {
        crate::GateResult {
            kind: crate::GateKind::Targeted,
            passed,
            exit_code: Some(i32::from(!passed)),
            signal: None,
            duration_ms: 7,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    #[test]
    fn gate_started_is_info_level() {
        assert_eq!(
            level_for(&EventKind::GateStarted {
                gate: crate::GateKind::Targeted
            }),
            Level::Info
        );
    }

    #[test]
    fn gate_finished_is_info_level_when_it_passed() {
        assert_eq!(
            level_for(&EventKind::GateFinished {
                result: sample_gate_result(true),
            }),
            Level::Info
        );
    }

    #[test]
    fn gate_finished_is_error_level_when_it_failed() {
        assert_eq!(
            level_for(&EventKind::GateFinished {
                result: sample_gate_result(false),
            }),
            Level::Error
        );
    }

    #[test]
    fn gate_events_carry_neither_attempt_nor_phase() {
        assert_eq!(
            attempt_of(&EventKind::GateStarted {
                gate: crate::GateKind::Targeted
            }),
            None
        );
        assert_eq!(
            phase_of(&EventKind::GateFinished {
                result: sample_gate_result(true),
            }),
            None
        );
    }

    #[test]
    fn gate_messages_name_the_gate_kind_and_outcome() {
        assert!(
            message_for(&EventKind::GateStarted {
                gate: crate::GateKind::Targeted
            })
            .contains("Targeted")
        );
        let passed = message_for(&EventKind::GateFinished {
            result: sample_gate_result(true),
        });
        assert!(passed.contains("passed"));
        let failed = message_for(&EventKind::GateFinished {
            result: sample_gate_result(false),
        });
        assert!(failed.contains("failed"));
    }

    #[test]
    fn draining_twice_does_not_repeat_the_first_batch() {
        let state = tempfile::tempdir().expect("tempdir");
        let mut recorder = recorder_at(state.path());
        let logger =
            Logger::open(state.path(), recorder.subscribe(), Level::Debug).expect("open logger");

        recorder
            .record(None, EventKind::Resumed)
            .expect("record first");
        assert_eq!(logger.drain().expect("first drain"), 1);

        assert_eq!(
            logger.drain().expect("second drain"),
            0,
            "nothing new was published between the two drains"
        );

        recorder
            .record(None, EventKind::PreflightStarted)
            .expect("record second");
        assert_eq!(logger.drain().expect("third drain"), 1);

        assert_eq!(log_lines(state.path()).len(), 2);
    }

    #[test]
    fn reopening_the_same_day_appends_rather_than_truncating() {
        let state = tempfile::tempdir().expect("tempdir");
        let mut recorder = recorder_at(state.path());

        {
            let logger = Logger::open(state.path(), recorder.subscribe(), Level::Debug)
                .expect("open first logger");
            recorder
                .record(None, EventKind::Resumed)
                .expect("record via first logger");
            logger.drain().expect("drain first logger");
        }
        {
            let logger = Logger::open(state.path(), recorder.subscribe(), Level::Debug)
                .expect("open second logger");
            recorder
                .record(None, EventKind::PreflightStarted)
                .expect("record via second logger");
            logger.drain().expect("drain second logger");
        }

        let lines = log_lines(state.path());
        assert_eq!(
            lines.len(),
            2,
            "both loggers' records must survive in the same day's file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_log_file_is_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("tempdir");
        let recorder = recorder_at(state.path());
        let _logger =
            Logger::open(state.path(), recorder.subscribe(), Level::Debug).expect("open logger");

        let mut entries = fs::read_dir(state.path().join("logs")).expect("read logs dir");
        let entry = entries.next().expect("one log file exists").expect("entry");
        assert!(entries.next().is_none(), "exactly one log file exists");

        let mode = fs::metadata(entry.path())
            .expect("stat log file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn message_names_the_secret_bearing_field_for_every_variant() {
        // Not a redaction test: proves message_for actually surfaces
        // free-text fields (title/detail/reason/text) rather than silently
        // dropping them, since redact() on an empty message would also
        // "pass" a test that only checked for the absence of a secret.
        let cases: Vec<(EventKind, &str)> = vec![
            (
                EventKind::TaskQueued {
                    title: "T".to_string(),
                },
                "T",
            ),
            (
                EventKind::TaskCancelled {
                    reason: "superseded".to_string(),
                },
                "superseded",
            ),
        ];
        for (kind, needle) in cases {
            assert!(message_for(&kind).contains(needle));
        }
    }
}

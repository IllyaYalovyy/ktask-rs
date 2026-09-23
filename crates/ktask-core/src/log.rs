//! The run log: every event, as one redacted JSON record per line.
//!
//! VISION.md §11 makes a run's log a privacy surface — "tokens, credentials, and
//! configured secret patterns are redacted from logs", "restrictive filesystem
//! permissions" — and §13 makes it the data behind the Logs screen: "raw and
//! structured views, search, filters, follow mode, error navigation", over output
//! that is "always attributable to a task, a phase and a moment in time". This
//! module is the write half of both sentences.
//!
//! # One JSON object per line, six columns
//!
//! Every line is `{"ts","level","task_id","attempt","phase","message"}`: the
//! moment, the attention the line asks for, and the three columns a screen
//! filters on. `docs/CONTRACT.md` gives the Logs screen "filter by level and
//! phase", which is only possible if both are columns rather than prose inside a
//! message, and a filter that reads prose is a filter that breaks when the prose
//! is reworded. Columns the moment does not supply are written `null` rather than
//! omitted: a reader that has to handle two shapes for one format reads the wrong
//! one eventually, and `null` is the true answer to "which task?" about an event
//! about the queue itself.
//!
//! [`serde_json`] writes the line from a typed record, so the structure is never
//! in a secret pattern's reach — which is why [`redact::redact`] is applied to the
//! message *text* here rather than [`redact::redact_json`] to a finished line.
//! Both are redaction inside the write path (ADR-0032); the difference is only
//! which half each one can touch, and here the half that holds the text is the
//! half that can hold a secret.
//!
//! # Nobody has to remember to log
//!
//! [`Logger::subscribe`] takes a ring from the [`Bus`] and writes what arrives on
//! a thread of its own, so a log exists for a run that was never told to write
//! one. VISION.md §3's rule — journal before the side effect — is the journal's
//! job; this is the human-readable half, and it is deliberately *behind* the
//! journal: [`Recorder::record`] commits a row before publishing it, so a line in
//! the log is a line that is already durable in SQLite. The consequence is the
//! one worth stating: the log is allowed to lose its last few lines to a power
//! cut and still be honest, because the journal is the record and the log is the
//! reading of it.
//!
//! # Levels, and what each one is for
//!
//! [`Level`] is the log's own vocabulary, not the state machine's: `Debug` is the
//! agent's output — the part of a run that is volume rather than fact, and the
//! reason a default-on `Debug` log would be an operator with a search box and no
//! signal — `Info` is ordinary lifecycle, `Warn` is a run that stopped or lost
//! something without anything failing, and `Error` is a check that refused or a
//! run waiting on a human. The mapping in `level_of` is exhaustive over the
//! catalog on purpose: an entry added by a later task has to be placed here, in
//! the open, rather than arrive at a level nobody chose.
//!
//! Filtering hides *lines*, never facts. The phase a task entered is remembered
//! from the moment the log reads [`EventKind::PhaseEntered`], whether or not that
//! line was written, so a run logged at `Error` still attributes its failures to
//! the phase that produced them.
//!
//! # What a run leaves behind
//!
//! `<state_dir>/logs/run-<date>.jsonl`, held at `0600` inside a `logs` directory
//! held at `0700`: the same modes the journal, the prompt library and an
//! attempt's evidence are kept to, because agent output is the least shareable
//! thing a run produces. The date is the day the log was opened; a run that
//! crosses midnight keeps writing to the file it started, because splitting one
//! run across two files would make "what happened in this run" a question with two
//! answers. Retention is VISION.md §11's `retention` question and is not decided
//! here.
//!
//! [`redact::redact`]: crate::redact::redact
//! [`redact::redact_json`]: crate::redact::redact_json
//! [`Recorder::record`]: crate::Recorder::record

use std::collections::HashMap;
use std::fmt::Display;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::ser::Error as _;
use time::ext::SystemTimeExt as _;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

use crate::redact::{check_patterns, redact};
use crate::{AttemptId, Bus, Error, Event, EventKind, Phase, Result, Subscription, TaskId};

/// The directory below a project's state directory that its logs live in.
const LOGS_DIR: &str = "logs";

/// The stem of one run log's filename, before the day it was opened.
const LOG_FILE_PREFIX: &str = "run-";

/// The extension of a run log's filename: newline-delimited JSON.
const LOG_FILE_SUFFIX: &str = "jsonl";

/// The mode of the `logs` directory: owner-only, the rule a state directory and
/// everything below it is kept to (VISION.md §11).
const LOGS_DIR_MODE: u32 = 0o700;

/// The mode of every log file. Agent output is the least shareable thing a run
/// produces, and a log a teammate can open is a secret a teammate has read.
const LOG_FILE_MODE: u32 = 0o600;

/// How long the writer waits for the next request before draining the ring on its
/// own clock.
///
/// The bus hands over records rather than a wake-up (`Subscription::drain` is a
/// poll, not a callback), so this is what bounds how long an event can sit in a
/// ring before it reaches the file. A flush does not wait for it: a request sent
/// to the writer arrives immediately.
const POLL: Duration = Duration::from_millis(50);

/// How much attention a line asks for, least to most.
///
/// The order is the type's meaning: a threshold keeps its level *and everything
/// above it*, so `Warn` means "the quiet lines are not worth reading right now".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// One line of agent output: the only part of a run that is volume rather
    /// than fact.
    Debug,
    /// Ordinary lifecycle — a task queued, an attempt started, a phase entered, a
    /// task done.
    Info,
    /// Something an operator will want to know, without a check having refused: a
    /// run that paused, was interrupted, was cancelled — or a log that had to give
    /// up events it never read.
    Warn,
    /// A check refused, or the run is waiting for a human to answer something.
    Error,
}

impl Level {
    /// The word this level is written as in the log's own `level` column.
    ///
    /// Lowercase, because the column is read by `jq` and by a filter a human
    /// types, and `error` matches what every other log on the machine says.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// The file one day's run writes: `<state_dir>/logs/run-<date>.jsonl`.
///
/// A run keeps the file it opened, so this answers "which file did that run
/// write?" for the day it started on and no other.
#[must_use]
pub fn log_path(state_dir: &Path, day: Date) -> PathBuf {
    state_dir
        .join(LOGS_DIR)
        .join(format!("{LOG_FILE_PREFIX}{day}.{LOG_FILE_SUFFIX}"))
}

/// The log of a run: the bus's own reader, writing to one file.
///
/// Opened by [`Logger::subscribe`] and closed by [`Logger::finish`] — or by being
/// dropped, which is what keeps a run that unwound from leaving a thread behind.
/// One logger per run is the intended use; two opened on the same day append to
/// the same file, each writing whole records, so a line is never half of one.
#[derive(Debug)]
pub struct Logger {
    /// The file this logger opened. It exists from the moment the logger does,
    /// empty until the first event arrives.
    path: PathBuf,
    /// The door to the writer, open for the logger's whole life.
    control: Mutex<Sender<Control>>,
    /// The writer, until [`Logger::finish`] or the drop that follows reaps it.
    worker: Mutex<Option<JoinHandle<Result<()>>>>,
}

/// What the run may ask its writer to do.
#[derive(Debug)]
enum Control {
    /// Write what the ring holds, sync it, and answer with what that produced.
    Flush(Sender<Result<()>>),
    /// Write what is left, sync it, and stop.
    Stop,
}

impl Logger {
    /// Follow every event `bus` publishes from now on, writing this day's log.
    ///
    /// The subscription is taken here, on this thread, so what the log sees is
    /// what a frontend that attached at the same moment would see — and nothing
    /// the caller does afterwards has to remember to log. `level` is the
    /// threshold: its level and above are written, below are read and folded but
    /// not written. `secret_patterns` are the configured extra shapes, checked
    /// here so a pattern that is not a regular expression is refused before a file
    /// exists rather than skipped at the moment a secret was being written.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when one of `secret_patterns` is not a regular
    /// expression, as [`check_patterns`] answers. [`Error::Serde`] when the clock
    /// names no instant this crate can hold, since a log has to be named for a day
    /// it cannot write. [`Error::NotFound`] when `state_dir` is not there, and
    /// [`Error::Policy`] when the state directory or its `logs` directory is a
    /// file or a link rather than a directory this module owns. [`Error::Io`] when
    /// the filesystem refused the directory or the file.
    pub fn subscribe(
        state_dir: &Path,
        level: Level,
        secret_patterns: &[String],
        bus: &Bus,
    ) -> Result<Self> {
        check_patterns(secret_patterns)?;
        let path = log_path(state_dir, clock_instant()?.date());
        let file = open_log(state_dir, &path)?;
        let (sender, receiver) = mpsc::channel();
        let subscription = bus.subscribe();
        let mut writer = Writer::new(file, level, secret_patterns.to_vec());
        let worker = thread::spawn(move || follow(subscription, &receiver, &mut writer));
        Ok(Self {
            path,
            control: Mutex::new(sender),
            worker: Mutex::new(Some(worker)),
        })
    }

    /// The file this logger writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write what the ring holds, put it on the storage, and answer once that is
    /// done.
    ///
    /// A test reads the log after this, and a run at a checkpoint calls it so the
    /// file says what the run has reached. An already-stopped writer is reported
    /// rather than assumed to have finished.
    ///
    /// # Errors
    ///
    /// The writer's own failure — [`Error::Io`] when the file refused a write or a
    /// sync, [`Error::Serde`] when a record had no JSON encoding or an instant no
    /// spelling — [`Error::Io`] naming the writer when it is no longer there to
    /// answer at all, which is reported rather than assumed away: nobody is
    /// writing the log any more, so it may be incomplete.
    pub fn flush(&self) -> Result<()> {
        let (ack, answer) = mpsc::channel();
        drop(locked(&self.control).send(Control::Flush(ack)));
        answer.recv().map_err(|_| writer_stopped())?
    }

    /// Write what is left, sync, and stop the writer.
    ///
    /// The log a run leaves behind is the one this answers for: everything the bus
    /// had published by now is in the file and on the storage by the time it
    /// returns.
    ///
    /// # Errors
    ///
    /// As [`Logger::flush`], for what was still in the ring; a writer that stopped
    /// by panicking is reported as [`Error::Io`] rather than re-panicked here.
    pub fn finish(self) -> Result<()> {
        self.close()
    }

    /// Ask the writer to stop, wait for it, and hand back what it decided.
    ///
    /// Shared by [`Logger::finish`] and the drop that follows it: the second call
    /// finds no writer left to reap and answers `Ok`, which is the truth — the
    /// first call already waited.
    fn close(&self) -> Result<()> {
        let writer = locked(&self.worker).take();
        drop(locked(&self.control).send(Control::Stop));
        writer.map_or(Ok(()), join_writer)
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Wait for the writer and answer with what it decided.
///
/// A writer that stopped by panicking is reported, not re-raised: a log writer
/// that died is a broken log, and a supervisor that unwinds over it loses the run
/// it was supervising to get the same answer.
fn join_writer(writer: JoinHandle<Result<()>>) -> Result<()> {
    writer.join().unwrap_or_else(|_panic| Err(writer_stopped()))
}

/// The refusal every "the writer is no longer there to answer" hands back.
fn writer_stopped() -> Error {
    Error::Io(std::io::Error::other(
        "the log writer stopped, so the log this logger was opened for is no longer being \
         written and may be incomplete",
    ))
}

/// Take a lock, keeping the logger usable if some other thread panicked while
/// holding it, for the reason `events.rs` gives its own helper.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The writer's own loop: drain the ring on the clock, answer a flush, stop when
/// asked.
fn follow(ring: Subscription, control: &Receiver<Control>, writer: &mut Writer) -> Result<()> {
    let mut ring = ring;
    loop {
        match control.recv_timeout(POLL) {
            Ok(Control::Flush(ack)) => drop(ack.send(settle(writer, &mut ring))),
            Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected) => {
                return settle(writer, &mut ring);
            }
            Err(RecvTimeoutError::Timeout) => settle(writer, &mut ring)?,
        }
    }
}

/// Take what the ring holds, write it, and put it on the storage.
///
/// The sync is what makes a flush worth waiting for, and an idle tick skips both
/// halves so a run that is thinking costs the storage nothing.
fn settle(writer: &mut Writer, subscription: &mut Subscription) -> Result<()> {
    let (events, lost) = subscription.drain();
    if events.is_empty() && lost == 0 {
        return Ok(());
    }
    writer.write_batch(&events, lost)?;
    writer.sync()
}

/// The file, the level it is written at, and where each task's run stands in it.
#[derive(Debug)]
struct Writer {
    /// The open log, positioned at the end.
    file: File,
    /// The threshold: below it a line is folded, not written.
    level: Level,
    /// The configured extra secret shapes, checked at [`Logger::subscribe`].
    patterns: Vec<String>,
    /// Per task, the attempt and phase the log has read most recently.
    running: HashMap<TaskId, Standing>,
}

/// Where one task's run stands, as far as the log has read so far.
#[derive(Debug, Default)]
struct Standing {
    /// The attempt the task is on, if an entry has named one.
    attempt: Option<AttemptId>,
    /// The phase the task entered, if an entry has named one.
    phase: Option<Phase>,
}

impl Writer {
    /// A writer over `file`, keeping `level` and above and redacting with
    /// `patterns`.
    fn new(file: File, level: Level, patterns: Vec<String>) -> Self {
        Self {
            file,
            level,
            patterns,
            running: HashMap::new(),
        }
    }

    /// Fold a batch into the file.
    ///
    /// Everything the batch yields goes out in one `write_all`, so a reader that
    /// catches the file mid-run — the Logs screen in follow mode, a `tail -f` —
    /// sees whole records rather than half of one.
    fn write_batch(&mut self, events: &[Event], lost: usize) -> Result<()> {
        let mut text = String::new();
        if lost > 0 {
            text.push_str(&self.gap(lost)?);
        }
        for event in events {
            self.fold(event, &mut text)?;
        }
        if text.is_empty() {
            return Ok(());
        }
        Ok(self.file.write_all(text.as_bytes())?)
    }

    /// Put what was written on the storage.
    fn sync(&mut self) -> Result<()> {
        self.file.sync_all().map_err(Into::into)
    }

    /// Append one event's record, unless its level is below the one this log keeps.
    fn fold(&mut self, event: &Event, out: &mut String) -> Result<()> {
        self.note(event);
        let level = level_of(&event.kind);
        if level < self.level {
            return Ok(());
        }
        let at = ts_text(event.ts)?;
        let message = redact(&message_of(&event.kind)?, &self.patterns);
        out.push_str(&record_text(&Record {
            ts: &at,
            level: level.as_str(),
            task_id: event.task_id,
            attempt: self.attempt_for(event),
            phase: self.phase_for(event),
            message: &message,
        })?);
        Ok(())
    }

    /// Record what this entry says about where its task's run now stands.
    ///
    /// Only the two entries that *say* can change the answer, and this runs before
    /// the level filter: a phase a threshold hid the line for is still the phase
    /// every later line happened in.
    fn note(&mut self, event: &Event) {
        let Some(task) = event.task_id else {
            return;
        };
        match &event.kind {
            EventKind::AttemptStarted { attempt, .. } => {
                self.running.entry(task).or_default().attempt = Some(*attempt);
            }
            EventKind::PhaseEntered { attempt, phase } => {
                let standing = self.running.entry(task).or_default();
                standing.attempt = Some(*attempt);
                standing.phase = Some(*phase);
            }
            _ => {}
        }
    }

    /// The attempt a line belongs to: the one its entry names, or the one the task
    /// was last seen on.
    fn attempt_for(&self, event: &Event) -> Option<AttemptId> {
        carried_attempt(&event.kind).or_else(|| self.standing(event).and_then(|run| run.attempt))
    }

    /// The phase a line belongs to: the one its entry names, or the one the task
    /// had entered.
    fn phase_for(&self, event: &Event) -> Option<Phase> {
        carried_phase(&event.kind).or_else(|| self.standing(event).and_then(|run| run.phase))
    }

    /// Where the line's task stood as of the lines already read.
    fn standing(&self, event: &Event) -> Option<&Standing> {
        event.task_id.and_then(|task| self.running.get(&task))
    }

    /// What the log says about events the bus gave up before it read them.
    ///
    /// A bounded ring is a real trade (VISION.md §5, ADR-0030) and a log that lost
    /// events without saying so is a false record rather than a short one, so the
    /// gap gets a line of its own. It is written whatever the threshold: a line
    /// about what the log cannot show is not a line anybody opted out of reading.
    fn gap(&self, lost: usize) -> Result<String> {
        let at = ts_text(clock_instant()?)?;
        let message = redact(&format!("RingLost given={lost}"), &self.patterns);
        record_text(&Record {
            ts: &at,
            level: Level::Warn.as_str(),
            task_id: None,
            attempt: None,
            phase: None,
            message: &message,
        })
    }
}

/// One line of the log, in the order the format documents its columns.
#[derive(Serialize)]
struct Record<'a> {
    /// When it happened, RFC 3339 in UTC — the journal's own spelling of the
    /// instant it stamped (ADR-0012).
    ts: &'a str,
    /// What it asks for, as [`Level`] spells it.
    level: &'a str,
    /// The task it is about, or `null` for one about the queue itself.
    task_id: Option<TaskId>,
    /// The attempt it belongs to, or `null` before one started.
    attempt: Option<AttemptId>,
    /// The phase it happened in, or `null` before one was entered.
    phase: Option<Phase>,
    /// The entry's name and the facts that say which, after redaction.
    message: &'a str,
}

/// `record` as one JSON line, ended.
fn record_text(record: &Record<'_>) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(record)?))
}

/// The level an entry is written at.
///
/// Exhaustive over the catalog so a later entry is *placed* rather than defaulted,
/// and the placement answers one question: who has to notice this? A gate is
/// placed by its own verdict rather than by its name, because a gate that passed
/// is ordinary lifecycle and a gate that refused is the reason the run stopped.
fn level_of(kind: &EventKind) -> Level {
    match kind {
        EventKind::AgentOutput { .. } => Level::Debug,
        EventKind::GateFinished { result } => {
            if result.passed {
                Level::Info
            } else {
                Level::Error
            }
        }
        EventKind::PreflightFailed { .. }
        | EventKind::VerifyFailed { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::DecisionRaised { .. } => Level::Error,
        EventKind::Paused { .. }
        | EventKind::Interrupted { .. }
        | EventKind::TaskCancelled { .. } => Level::Warn,
        EventKind::TaskQueued { .. }
        | EventKind::PreflightStarted
        | EventKind::PreflightPassed { .. }
        | EventKind::AttemptStarted { .. }
        | EventKind::PhaseEntered { .. }
        | EventKind::GateStarted { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::PublishStarted { .. }
        | EventKind::PublishVerified { .. }
        | EventKind::TaskDone { .. }
        | EventKind::Resumed
        | EventKind::RecoveryDecision { .. }
        | EventKind::TddExceptionUsed { .. }
        | EventKind::GateAcknowledged { .. }
        | EventKind::AttemptRecorded { .. }
        | EventKind::AttemptFinished { .. }
        | EventKind::SelfHealingReport { .. } => Level::Info,
    }
}

/// The attempt an entry names in its own payload, when it names one.
fn carried_attempt(kind: &EventKind) -> Option<AttemptId> {
    match kind {
        // Each of these names one run in its payload, so the run is a column of
        // the line rather than prose inside it — including a recovery's account,
        // whose attempt is the remediation it accounts for.
        EventKind::AttemptStarted { attempt, .. }
        | EventKind::PhaseEntered { attempt, .. }
        | EventKind::AgentOutput { attempt, .. }
        | EventKind::AttemptFinished { attempt, .. }
        | EventKind::VerifyPassed { attempt }
        | EventKind::VerifyFailed { attempt, .. }
        | EventKind::PublishStarted { attempt, .. }
        | EventKind::SelfHealingReport { attempt, .. } => Some(*attempt),
        _ => None,
    }
}

/// The phase an entry names in its own payload, when it names one.
fn carried_phase(kind: &EventKind) -> Option<Phase> {
    match kind {
        EventKind::PhaseEntered { phase, .. } | EventKind::Interrupted { phase } => Some(*phase),
        _ => None,
    }
}

/// The entry as one readable line: its name, then the payload's facts that are not
/// already columns of their own.
///
/// An entry whose whole payload is the two columns gets its name and nothing
/// more: `attempt` and `phase` are what a screen filters on, and repeating them
/// inside prose invites the two halves to disagree. A gate's output is deliberately
/// absent — it is the journal's and the evidence directory's to hold (ADR-0037),
/// and a log line that carried it would be the reason a `Warn` log was unreadable.
fn message_of(kind: &EventKind) -> Result<String> {
    let facts = match kind {
        EventKind::PreflightFailed { class, detail }
        | EventKind::VerifyFailed { class, detail, .. }
        | EventKind::TaskFailed { class, detail } => format!("class={class:?} detail={detail}"),
        // The entries whose payload *is* a column: `PhaseEntered` names the phase
        // the line is filed under, `VerifyPassed` the attempt that passed,
        // `Interrupted` the phase it stopped in. `PreflightStarted` and `Resumed`
        // carry nothing at all.
        EventKind::PhaseEntered { .. }
        | EventKind::VerifyPassed { .. }
        | EventKind::Interrupted { .. }
        | EventKind::PreflightStarted
        | EventKind::Resumed => String::new(),
        EventKind::TaskQueued { title } => format!("title={title}"),
        EventKind::PreflightPassed { base_sha } => format!("base={base_sha}"),
        EventKind::AttemptStarted {
            protocol,
            pid,
            base_sha,
            ..
        } => format!("protocol={protocol} pid={pid} base={base_sha}"),
        EventKind::AgentOutput { stream, text, .. } => format!("stream={stream:?} text={text}"),
        EventKind::AttemptFinished {
            exit_code,
            usage,
            session_id,
            model_reported,
            ..
        } => format!(
            "exit={exit_code} usage={usage:?} session={session_id:?} model={model_reported:?}"
        ),
        EventKind::GateStarted { gate } => format!("gate={gate}"),
        EventKind::GateFinished { result } => format!(
            "gate={} passed={} exit={:?} signal={:?} ms={} timed_out={}",
            result.kind,
            result.passed,
            result.exit_code,
            result.signal,
            result.duration_ms,
            result.timed_out
        ),
        EventKind::PublishStarted { candidate_sha, .. } => format!("candidate={candidate_sha}"),
        EventKind::PublishVerified { commit, remote_sha } => {
            format!("commit={commit} remote={remote_sha}")
        }
        EventKind::TaskDone { commit } => format!("commit={commit}"),
        EventKind::TaskCancelled { reason } => format!("reason={reason}"),
        EventKind::Paused { reason } => format!("reason={reason:?}"),
        EventKind::RecoveryDecision { decision, detail } => {
            format!("decision={decision:?} detail={detail}")
        }
        EventKind::TddExceptionUsed { exception, reason } => {
            format!("exception={exception:?} reason={reason}")
        }
        EventKind::DecisionRaised { request } => format!("question={}", request.question),
        EventKind::GateAcknowledged { by, at } => format!("by={by} at={}", ts_text(*at)?),
        EventKind::AttemptRecorded { record } => format!(
            "id={} exit={} candidate={:?}",
            record.id, record.exit_reason, record.candidate_sha
        ),
        // The account a recovery leaves behind: the three-part answer VISION.md
        // §7 asks it to hold. The attempt it names is the line's own `attempt`
        // column, so it is not written twice.
        EventKind::SelfHealingReport {
            class,
            repairs,
            outcome,
            ..
        } => format!("class={class:?} repairs={repairs:?} outcome={outcome}"),
    };
    Ok(if facts.is_empty() {
        kind.discriminant().to_owned()
    } else {
        format!("{} {facts}", kind.discriminant())
    })
}

/// Open the day's log for appending, at [`LOG_FILE_MODE`].
fn open_log(state_dir: &Path, path: &Path) -> Result<File> {
    ensure_state_directory(state_dir)?;
    private_dir(&state_dir.join(LOGS_DIR))?;
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(LOG_FILE_MODE)
        .open(path)?;
    fs::set_permissions(path, Permissions::from_mode(LOG_FILE_MODE))?;
    Ok(file)
}

/// Require the state directory a registration made to be the directory it claims.
///
/// A log below a state directory that is not there would be written wherever the
/// path happened to resolve, which is the property VISION.md §11's "no `.ktask/`
/// in project repositories" exists to keep true.
fn ensure_state_directory(state_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(state_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(occupied(state_dir)),
        Err(why) if refused_because_absent(&why) => Err(Error::NotFound {
            what: format!("state directory `{}`", state_dir.display()),
        }),
        Err(why) => Err(why.into()),
    }
}

/// Make `path` exist as a directory held at [`LOGS_DIR_MODE`].
///
/// The mode is set as well as asked for at creation: a directory that already
/// exists keeps whatever mode somebody gave it, and only the set takes a grant
/// back. A link is refused as an occupied path, since writing through one would
/// put a run's log wherever the link points.
fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(occupied(path)),
        Err(why) if refused_because_absent(&why) => {
            DirBuilder::new().mode(LOGS_DIR_MODE).create(path)?;
        }
        Err(why) => return Err(why.into()),
    }
    fs::set_permissions(path, Permissions::from_mode(LOGS_DIR_MODE))?;
    Ok(())
}

/// Whether the filesystem says the path simply is not there.
///
/// `NotADirectory` is the same answer for a writer: a level above the path is not
/// a directory, so no log can be below it. `attempt.rs` gives the reason for the
/// pair; this module reads the filesystem the same way it does.
fn refused_because_absent(why: &std::io::Error) -> bool {
    matches!(
        why.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Refuse a level of the layout that is there and is not a directory.
fn occupied(path: &Path) -> Error {
    Error::Policy {
        detail: format!(
            "`{}` is already there and is not a directory",
            path.display()
        ),
        paths: vec![path.to_path_buf()],
    }
}

/// The clock, read now.
///
/// The log's own reading, for the two facts only a clock can supply: the day the
/// file is named for, and the moment of a line the bus could no longer hand over.
/// Setting a machine's clock is not something a test may do, so every decision
/// made from a reading lives above [`instant_of`], which takes one as an argument.
fn clock_instant() -> Result<OffsetDateTime> {
    instant_of(SystemTime::now())
}

/// One clock reading as an instant, refused when it names none `time` can hold.
///
/// The sign is the part worth a test: `duration_since` reports the *distance* from
/// the epoch whichever way the reading lies, so a clock set before the epoch has to
/// keep the side of it it was read on. `checked_add` is what refuses the readings
/// whose date no calendar `time` holds rather than saturating them into a
/// plausible-looking year.
fn instant_of(reading: SystemTime) -> Result<OffsetDateTime> {
    let since_epoch = reading.signed_duration_since(UNIX_EPOCH);
    OffsetDateTime::UNIX_EPOCH
        .checked_add(since_epoch)
        .ok_or_else(|| no_text(&format!("{reading:?}"), &"no calendar `time` can hold"))
}

/// The instant as the log's `ts` column holds it: RFC 3339, in UTC.
///
/// This is the one spelling [`crate::Event`] writes its own `ts` in (ADR-0012), so
/// the log and the journal cannot disagree about when one thing happened, and an
/// instant authored at a non-zero offset is resolved to UTC before it is written.
/// ADR-0012's decision — a missing spelling is a serialization failure, not a
/// panic — is followed here for the reason the journal gives: a supervisor that
/// panics loses the run it was supervising.
fn ts_text(instant: OffsetDateTime) -> Result<String> {
    let Some(utc) = instant.checked_to_offset(UtcOffset::UTC) else {
        return Err(no_text(
            &format!("{instant:?}"),
            &"no UTC offset to write it at",
        ));
    };
    utc.format(&Rfc3339)
        .map_err(|unformattable| no_text(&format!("{instant:?}"), &unformattable))
}

/// The refusal both clock helpers hand back: what had no spelling, and why.
fn no_text(subject: &str, reason: &dyn Display) -> Error {
    Error::Serde(serde_json::Error::custom(format_args!(
        "{subject} has no RFC 3339 spelling to write in the log: {reason}"
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_FILE_MODE, LOGS_DIR_MODE, Level, Logger, Record, instant_of, level_of, log_path,
        message_of, record_text, ts_text,
    };
    use crate::redact::MASK;
    use crate::redact::fixtures::github_token;
    use crate::{
        AttemptId, AttemptRecord, Bus, DecisionRequest, Error, Event, EventKind, EventSeq,
        FailureClass, GateKind, GateResult, Journal, PauseReason, Phase, Recorder, Recovery,
        Stream, TaskId, TddException, Usage, UsageSource,
    };
    use proptest::prelude::*;
    use serde_json::Value;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::{TempDir, tempdir};
    use time::format_description::well_known::Rfc3339;
    use time::macros::{date, datetime, format_description};
    use time::{Date, OffsetDateTime, Time};

    /// No configured extra patterns: what a project that named none hands every
    /// record, which is the case the built-in table alone has to cover.
    const NO_EXTRA: &[String] = &[];

    /// The commit a message is expected to name, so a field that never reached
    /// the text is visible rather than mistaken for a placeholder.
    const SHA: &str = "0b78d3f1c2a4";

    /// The instant every event below is stamped with. A test that asserts on the
    /// `ts` column asserts on a known instant, never on the wall clock.
    const THEN: OffsetDateTime = datetime!(2026-09-21 09:14:03 UTC);

    /// The six columns, in the order the format documents them.
    const COLUMNS: [&str; 6] = ["ts", "level", "task_id", "attempt", "phase", "message"];

    /// A scratch parent below the system temp directory: `docs/DESIGN.md`
    /// Conventions forbids a test from writing inside the repository.
    fn scratch() -> TempDir {
        tempdir().expect("a scratch directory below the system temp directory")
    }

    /// The state directory of a registered project. [`Logger::subscribe`] insists
    /// one exists, because a log written below a path that is not there would land
    /// wherever that path happened to resolve.
    fn state_dir(parent: &TempDir) -> PathBuf {
        let dir = parent.path().join("state");
        fs::create_dir(&dir).expect("a scratch state directory");
        dir
    }

    /// Open the day's log at `level`, following `bus` with no configured patterns.
    fn log_at(state: &Path, level: Level, bus: &Bus) -> Logger {
        Logger::subscribe(state, level, NO_EXTRA, bus)
            .expect("a log opens below a state directory that exists")
    }

    /// Hand one event to the bus as the journal would have: an envelope with a
    /// sequence and an instant, not a bare catalog entry.
    fn publish(bus: &Bus, task: Option<TaskId>, kind: EventKind) {
        bus.publish(Event {
            seq: EventSeq::new(1),
            ts: THEN,
            task_id: task,
            kind,
        });
    }

    /// The task every event below belongs to.
    fn task() -> TaskId {
        TaskId::new(7)
    }

    /// The log's bytes, exactly as the file holds them: what a `grep` for a secret
    /// would read, and the only thing that answers "did it reach disk".
    fn raw(path: &Path) -> String {
        fs::read_to_string(path).expect("the log a run left behind is readable")
    }

    /// Every record the log holds, oldest first. A line that is not JSON fails
    /// here, because "every line parses" is this module's done-when rather than
    /// one test's assertion, and repeating that assertion per test is how one of
    /// them comes to omit it.
    fn records(path: &Path) -> Vec<Value> {
        raw(path)
            .lines()
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|malformed| {
                    panic!("`{line}` is not the JSON record the format promises: {malformed}")
                })
            })
            .collect()
    }

    /// The record's `message` column, which is what the content of a line is read
    /// from.
    fn message(record: &Value) -> &str {
        record["message"]
            .as_str()
            .expect("the message column is written as text")
    }

    /// The mode a path is kept at, with nothing above the permission bits.
    fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("the path is there to be looked at")
            .permissions()
            .mode()
            & 0o777
    }

    /// One gate's record, passed or refused, with output a log must not carry.
    fn gate(passed: bool) -> GateResult {
        GateResult {
            kind: GateKind::Verify,
            passed,
            exit_code: Some(i32::from(passed)),
            signal: None,
            duration_ms: 1_845_003,
            stdout: "test result: FAILED. 2 passed; 1 failed".to_owned(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    #[test]
    fn every_published_event_becomes_exactly_one_json_record() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Debug, &bus);

        publish(
            &bus,
            None,
            EventKind::TaskQueued {
                title: "Write the run log".to_owned(),
            },
        );
        publish(
            &bus,
            Some(task()),
            EventKind::TaskDone {
                commit: SHA.to_owned(),
            },
        );
        publish(&bus, Some(task()), EventKind::PreflightStarted);
        logger.flush().expect("a flush reaches the writer");

        let written = records(logger.path());
        assert_eq!(
            written.len(),
            3,
            "one record per published event, and no line written for nothing"
        );
        for record in &written {
            let mut actual = record
                .as_object()
                .expect("a record is a JSON object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<&str>>();
            let mut documented = COLUMNS.to_vec();
            documented.sort_unstable();
            actual.sort_unstable();
            assert_eq!(
                actual, documented,
                "a record carries exactly the six documented columns, no more"
            );
        }
    }

    #[test]
    fn the_columns_are_written_in_the_order_the_format_documents() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);
        publish(
            &bus,
            Some(task()),
            EventKind::TaskDone {
                commit: SHA.to_owned(),
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let log = raw(logger.path());
        let line = log.lines().next().expect("the record was written");
        let mut previous = 0;
        for column in COLUMNS {
            let key = format!("\"{column}\":");
            let at = line
                .find(&key)
                .unwrap_or_else(|| panic!("`{column}` is missing from `{line}`"));
            assert!(at > previous, "`{column}` is out of order in `{line}`");
            previous = at;
        }
        assert!(
            log.ends_with('\n'),
            "a record is ended by its newline, so a reader following the file never \
             waits on a half line: {log}"
        );
    }

    #[test]
    fn the_log_writes_the_instant_the_journal_stamped_not_a_fresh_reading() {
        let parent = scratch();
        let state = state_dir(&parent);
        let database = state.join("journal.db");
        let journal = Journal::open(&database).expect("a journal opens below the scratch state");
        let bus = Bus::new();
        let mut recorder = Recorder::with_bus(
            Journal::open(&database).expect("a second handle to the same journal"),
            bus.clone(),
        );
        let logger = log_at(&state, Level::Info, &bus);

        let seq = recorder
            .record(
                Some(task()),
                EventKind::TaskDone {
                    commit: SHA.to_owned(),
                },
            )
            .expect("the journal takes the record");
        logger.flush().expect("a flush reaches the writer");

        let journaled = journal
            .event(seq)
            .expect("the journal is readable")
            .expect("the record just appended is in the file that committed it");
        let written = records(logger.path());
        assert_eq!(written.len(), 1, "one event, one record");
        assert_eq!(
            written[0]["ts"],
            Value::String(
                journaled
                    .ts
                    .format(&Rfc3339)
                    .expect("the instant the journal stamped has a spelling")
            ),
            "one event has one moment in both files: the log writes the instant the \
             journal stamped, not a second reading of the clock"
        );
    }

    #[test]
    fn a_planted_token_does_not_reach_the_log_file() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Debug, &bus);
        let token = github_token();

        publish(
            &bus,
            Some(task()),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: format!("export GITHUB_TOKEN={token} && git push"),
            },
        );
        publish(
            &bus,
            Some(task()),
            EventKind::TaskFailed {
                class: FailureClass::EnvironmentFailure,
                detail: format!("the remote refused the credential {token}"),
            },
        );
        let path = logger.path().to_path_buf();
        logger
            .finish()
            .expect("finishing writes what the run published");

        let written = raw(&path);
        assert!(
            !written.contains(&token),
            "a planted credential reached the log the run leaves behind: {written}"
        );
        assert!(
            written.contains(MASK),
            "a redacted line says a secret was there, so a reader knows the line is \
             not complete: {written}"
        );
        assert_eq!(
            records(&path).len(),
            2,
            "redaction shortens a record, it never loses one"
        );
    }

    #[test]
    fn a_configured_pattern_is_redacted_from_the_log_too() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let patterns = ["ACME-[0-9]+".to_owned()];
        let logger = Logger::subscribe(&state, Level::Debug, &patterns, &bus)
            .expect("a configured pattern set opens a log");

        publish(
            &bus,
            Some(task()),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "closing ticket ACME-4711 for the customer".to_owned(),
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let written = raw(logger.path());
        assert!(
            !written.contains("ACME-4711"),
            "the configured pattern was skipped: {written}"
        );
        assert!(written.contains(MASK), "the mask replaced it: {written}");
        assert!(
            written.contains("for the customer"),
            "a record that loses the shape and keeps the prose is what an operator \
             can still read: {written}"
        );
    }

    #[test]
    fn a_pattern_that_is_not_a_regular_expression_is_refused_before_a_file_exists() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();

        let refused = Logger::subscribe(&state, Level::Info, &["(".to_owned()], &bus)
            .expect_err("a pattern that cannot be compiled cannot be honoured either");
        assert!(
            matches!(refused, Error::Config { .. }),
            "the refusal names the configuration the operator has to fix: {refused}"
        );
        assert!(
            !state.join("logs").exists(),
            "a run that cannot redact leaves no log for anyone to trust, so the \
             refusal comes before the file"
        );
    }

    #[test]
    fn the_log_and_its_directory_are_kept_to_their_owner() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        assert_eq!(
            mode(logger.path()),
            LOG_FILE_MODE,
            "agent output is the least shareable thing a run produces, so a \
             teammate cannot open the log"
        );
        assert_eq!(
            mode(&state.join("logs")),
            LOGS_DIR_MODE,
            "the directory is held to the same rule as the state directory above it"
        );
    }

    #[test]
    fn a_logs_directory_someone_left_world_readable_is_tightened() {
        let parent = scratch();
        let state = state_dir(&parent);
        let logs = state.join("logs");
        fs::create_dir(&logs).expect("a scratch logs directory");
        fs::set_permissions(&logs, Permissions::from_mode(0o755)).expect("the mode is set");

        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        assert_eq!(
            mode(&logs),
            LOGS_DIR_MODE,
            "a directory that already exists keeps whatever mode somebody gave it \
             unless the open takes the grant back"
        );
        assert_eq!(mode(logger.path()), LOG_FILE_MODE, "the file it opened too");
    }

    #[test]
    fn a_log_file_someone_left_world_readable_is_tightened() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();

        let first = log_at(&state, Level::Info, &bus);
        let path = first.path().to_path_buf();
        first.flush().expect("a flush reaches the writer");
        fs::set_permissions(&path, Permissions::from_mode(0o644))
            .expect("a scratch mode to refuse");
        assert_eq!(
            mode(&path),
            0o644,
            "the run log a teammate could read is the state this refuses to leave behind"
        );

        let second = log_at(&state, Level::Info, &bus);
        assert_eq!(
            mode(second.path()),
            LOG_FILE_MODE,
            "opening the day's log for appending takes back a grant somebody else \
             gave the file, because a mode asked for at creation alone is a mode a \
             `chmod` undoes"
        );
    }

    #[test]
    fn a_run_that_published_nothing_still_leaves_the_log_it_owns() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);
        let path = logger.path().to_path_buf();

        logger.finish().expect("an empty log finishes");

        assert_eq!(
            fs::read(&path).expect("the log is there").len(),
            0,
            "a run that said nothing leaves an empty log rather than no log, so the \
             answer to \"where is the log\" is never \"nowhere\""
        );
    }

    #[test]
    fn the_file_is_named_for_an_iso_day_below_the_logs_directory() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        let name = logger
            .path()
            .file_name()
            .expect("the log is a file, not a directory")
            .to_string_lossy()
            .into_owned();
        let stem = name
            .strip_prefix("run-")
            .and_then(|rest| rest.strip_suffix(".jsonl"))
            .unwrap_or_else(|| panic!("`{name}` is not `run-<date>.jsonl`"));
        let day = Date::parse(stem, &format_description!("[year]-[month]-[day]"))
            .unwrap_or_else(|unparsable| panic!("`{stem}` is not an ISO day: {unparsable}"));
        assert_eq!(
            logger
                .path()
                .parent()
                .expect("the log is below a directory"),
            state.join("logs"),
            "the log lives below `<state_dir>/logs`"
        );
        assert_eq!(
            log_path(&state, day),
            logger.path(),
            "one day names one file, and the answer to \"which file did that run \
             write\" is the same function the open used"
        );
    }

    #[test]
    fn log_path_names_the_file_below_the_state_directory() {
        assert_eq!(
            log_path(Path::new("/state/1a2b3c4d5e6f7a8b"), date!(2026 - 09 - 21)),
            PathBuf::from("/state/1a2b3c4d5e6f7a8b/logs/run-2026-09-21.jsonl"),
            "the shape `VISION.md` §11 gives a run's log, spelled exactly"
        );
    }

    #[test]
    fn every_catalog_entry_is_placed_at_the_level_its_attention_deserves() {
        let placed = every_entry();
        let mut names = placed
            .iter()
            .map(|(kind, _)| kind.discriminant())
            .collect::<Vec<&str>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            26,
            "the catalog holds 26 entries and this table places every one, so an \
             entry a later task adds has to be placed here as well"
        );

        for (kind, expected) in &placed {
            assert_eq!(
                level_of(kind),
                *expected,
                "`{}` is written at the level that says who has to notice it",
                kind.discriminant()
            );
        }
    }

    #[test]
    fn a_threshold_keeps_only_the_levels_at_and_above_it() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Warn, &bus);

        publish(
            &bus,
            Some(task()),
            EventKind::TaskQueued {
                title: "Filter the log".to_owned(),
            },
        );
        publish(
            &bus,
            Some(task()),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "a line of agent output".to_owned(),
            },
        );
        publish(
            &bus,
            Some(task()),
            EventKind::TaskCancelled {
                reason: "an operator stopped it".to_owned(),
            },
        );
        publish(
            &bus,
            Some(task()),
            EventKind::TaskFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "no remote".to_owned(),
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let written = records(logger.path());
        let levels = written
            .iter()
            .map(|record| record["level"].as_str().expect("a level is text"))
            .collect::<Vec<&str>>();
        assert_eq!(
            levels,
            ["warn", "error"],
            "a `Warn` threshold hides the debug and info lines and keeps everything \
             from its own level up"
        );
    }

    /// One catalog entry beside the level it is written at.
    type Placement = (EventKind, Level);
    /// Tag every entry of one level's list with that level.
    fn at(level: Level, kinds: impl IntoIterator<Item = EventKind>) -> Vec<Placement> {
        kinds.into_iter().map(|kind| (kind, level)).collect()
    }

    /// Every entry in the catalog, grouped by the level [`super::level_of`] places
    /// it at.
    ///
    /// The group a entry is listed in *is* the expectation the test compares
    /// against. `GateFinished` is listed twice, once under each verdict, because
    /// its level is what the gate decided rather than what the entry is called —
    /// which is why the pairs outnumber the catalog's entries by one.
    fn every_entry() -> Vec<Placement> {
        let mut placed = at(Level::Debug, debug_lines());
        placed.extend(at(Level::Info, info_lines()));
        placed.extend(at(Level::Warn, warn_lines()));
        placed.extend(at(Level::Error, error_lines()));
        placed
    }

    /// The entry that is volume rather than fact: an agent's own line.
    fn debug_lines() -> Vec<EventKind> {
        vec![EventKind::AgentOutput {
            attempt: AttemptId::new(3),
            stream: Stream::Stdout,
            text: "reading the module".to_owned(),
        }]
    }

    /// The entries that say what a run did, with nothing asking for attention.
    fn info_lines() -> Vec<EventKind> {
        vec![
            EventKind::TaskQueued {
                title: "Log the run".to_owned(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: SHA.to_owned(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(3),
                protocol: "tdd".to_owned(),
                pid: 4711,
                base_sha: SHA.to_owned(),
            },
            EventKind::PhaseEntered {
                attempt: AttemptId::new(3),
                phase: Phase::Red,
            },
            EventKind::AttemptFinished {
                attempt: AttemptId::new(3),
                exit_code: 0,
                usage: Some(Usage {
                    input_tokens: Some(8_120),
                    output_tokens: Some(1_944),
                    cached_tokens: Some(6_400),
                    cost_usd: Some(0.42),
                    source: UsageSource::Provider,
                }),
                session_id: Some("sess_01HQZK".to_owned()),
                model_reported: Some("gpt-5.6-sol".to_owned()),
            },
            EventKind::GateStarted {
                gate: GateKind::Verify,
            },
            EventKind::GateFinished { result: gate(true) },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(3),
            },
            EventKind::PublishStarted {
                attempt: AttemptId::new(3),
                candidate_sha: SHA.to_owned(),
            },
            EventKind::PublishVerified {
                commit: SHA.to_owned(),
                remote_sha: SHA.to_owned(),
            },
            EventKind::TaskDone {
                commit: SHA.to_owned(),
            },
            EventKind::Resumed,
            EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "the journal ends mid-phase".to_owned(),
            },
            EventKind::TddExceptionUsed {
                exception: TddException::Documentation,
                reason: "documentation only".to_owned(),
            },
            EventKind::GateAcknowledged {
                by: "ops@example.com".to_owned(),
                at: THEN,
            },
            EventKind::AttemptRecorded {
                record: Box::new(record()),
            },
            EventKind::SelfHealingReport {
                attempt: AttemptId::new(2),
                class: FailureClass::VerificationFailure,
                repairs: vec!["re-run the fmt gate".to_owned()],
                outcome: "green on the rerun".to_owned(),
            },
        ]
    }

    /// The entries that stopped a run without a check refusing it.
    fn warn_lines() -> Vec<EventKind> {
        vec![
            EventKind::TaskCancelled {
                reason: "an operator stopped it".to_owned(),
            },
            EventKind::Paused {
                reason: PauseReason::Limit { until: None },
            },
            EventKind::Interrupted {
                phase: Phase::Implement,
            },
        ]
    }

    /// The entries that say a check refused, or that a human owes the run an answer.
    fn error_lines() -> Vec<EventKind> {
        vec![
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "no remote".to_owned(),
            },
            EventKind::GateFinished {
                result: gate(false),
            },
            EventKind::VerifyFailed {
                attempt: AttemptId::new(3),
                class: FailureClass::VerificationFailure,
                detail: "a test failed".to_owned(),
            },
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "the agent gave up".to_owned(),
            },
            EventKind::DecisionRaised { request: request() },
        ]
    }

    /// A decision a human has to answer, for the entry that asks for one.
    fn request() -> DecisionRequest {
        DecisionRequest {
            question: "Which column owns the phase?".to_owned(),
            options: vec!["column".to_owned(), "message".to_owned()],
            tradeoffs: "A column is filterable, prose is not.".to_owned(),
            impact: "The Logs screen's filter.".to_owned(),
            recommended: Some("column".to_owned()),
        }
    }

    /// An attempt's whole evidence, for the one entry whose payload is a record.
    fn record() -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(3),
            task: task(),
            started: THEN,
            ended: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            exit_reason: "exited 0".to_owned(),
            gates: Vec::new(),
            usage: None,
            base_sha: SHA.to_owned(),
            candidate_sha: None,
        }
    }

    /// What a gap line says it lost, which is the only number it carries.
    fn given(record: &Value) -> usize {
        message(record)
            .strip_prefix("RingLost given=")
            .unwrap_or_else(|| panic!("`{}` is not the gap line that owes it", message(record)))
            .parse()
            .expect("a gap line counts what it lost in digits")
    }

    /// The earliest instant `time` holds, which [`super::ts_text`] has no digits
    /// for: a negative year is a real moment whose year cannot be written in the
    /// four RFC 3339 asks for.
    fn before_the_common_era() -> OffsetDateTime {
        Date::MIN.with_time(Time::MIDNIGHT).assume_utc()
    }

    /// Start an attempt and enter a phase, as the two entries that say so do.
    fn start_attempt(bus: &Bus, attempt: AttemptId, phase: Phase) {
        publish(
            bus,
            Some(task()),
            EventKind::AttemptStarted {
                attempt,
                protocol: "tdd".to_owned(),
                pid: 4711,
                base_sha: SHA.to_owned(),
            },
        );
        publish(
            bus,
            Some(task()),
            EventKind::PhaseEntered { attempt, phase },
        );
    }

    #[test]
    fn filtering_hides_lines_never_the_phase_a_later_line_happened_in() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Error, &bus);

        start_attempt(&bus, AttemptId::new(2), Phase::Red);
        publish(
            &bus,
            Some(task()),
            EventKind::TaskFailed {
                class: FailureClass::VerificationFailure,
                detail: "the gate refused".to_owned(),
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let written = records(logger.path());
        assert_eq!(
            written.len(),
            1,
            "the two `Info` lines are below the threshold and are not written"
        );
        assert_eq!(
            message(&written[0]),
            "TaskFailed class=VerificationFailure detail=the gate refused",
            "the entry that stopped the run says which class of failure it was"
        );
        assert_eq!(
            written[0]["attempt"].as_u64(),
            Some(2),
            "the attempt is read from a line the threshold hid, because filtering \
             hides lines and not facts"
        );
        assert_eq!(
            written[0]["phase"].as_str(),
            Some("Red"),
            "a failure is attributed to the phase that produced it even when the \
             line naming that phase was never written"
        );
    }

    #[test]
    fn a_line_is_attributed_to_the_task_attempt_and_phase_it_came_in_on() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Debug, &bus);

        start_attempt(&bus, AttemptId::new(1), Phase::Green);
        publish(
            &bus,
            Some(task()),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stderr,
                text: "a line the agent complained".to_owned(),
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let written = records(logger.path());
        let output = written
            .last()
            .expect("the agent's line is the last one written");
        assert_eq!(output["level"].as_str(), Some("debug"));
        assert_eq!(
            output["task_id"].as_u64(),
            Some(7),
            "VISION.md §13's screen filters by task, which only a column can answer"
        );
        assert_eq!(output["attempt"].as_u64(), Some(1));
        assert_eq!(output["phase"].as_str(), Some("Green"));
        assert_eq!(
            message(output),
            "AgentOutput stream=Stderr text=a line the agent complained"
        );
    }

    #[test]
    fn the_attempt_a_line_names_wins_over_the_one_the_log_remembered() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Debug, &bus);

        start_attempt(&bus, AttemptId::new(1), Phase::Red);
        publish(
            &bus,
            Some(task()),
            EventKind::AgentOutput {
                attempt: AttemptId::new(2),
                stream: Stream::Stdout,
                text: "output from a retry".to_owned(),
            },
        );
        publish(
            &bus,
            Some(task()),
            EventKind::Interrupted {
                phase: Phase::Publish,
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let written = records(logger.path());
        let output = &written[written.len() - 2];
        assert_eq!(
            output["attempt"].as_u64(),
            Some(2),
            "the attempt in the payload is the fact; the remembered one is a fallback"
        );
        assert_eq!(
            output["phase"].as_str(),
            Some("Red"),
            "a line that names no phase is still filed under the one its task \
             entered"
        );
        let interrupted = written
            .last()
            .expect("the interruption is the last line written");
        assert_eq!(interrupted["attempt"].as_u64(), Some(1));
        assert_eq!(
            interrupted["phase"].as_str(),
            Some("Publish"),
            "the phase a signal stopped the run in is not the phase it entered \
             earlier, however stale the remembered one looks beside it"
        );
    }

    #[test]
    fn a_line_about_the_queue_itself_has_no_task_to_attribute() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        publish(
            &bus,
            None,
            EventKind::TaskQueued {
                title: "A task nobody owns yet".to_owned(),
            },
        );
        logger.flush().expect("a flush reaches the writer");

        let written = records(logger.path());
        assert_eq!(written.len(), 1);
        for column in ["task_id", "attempt", "phase"] {
            assert!(
                written[0][column].is_null(),
                "`{column}` is written `null` rather than omitted, because `null` is \
                 the true answer to \"which task?\" about an entry about the queue"
            );
        }
        assert_eq!(
            message(&written[0]),
            "TaskQueued title=A task nobody owns yet"
        );
    }

    #[test]
    fn events_the_ring_gave_up_are_counted_in_the_log() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::with_capacity(0);
        let logger = log_at(&state, Level::Debug, &bus);

        for line in 0..3 {
            publish(
                &bus,
                Some(task()),
                EventKind::AgentOutput {
                    attempt: AttemptId::new(1),
                    stream: Stream::Stdout,
                    text: format!("line {line}"),
                },
            );
        }
        let path = logger.path().to_path_buf();
        logger
            .finish()
            .expect("finishing writes what the run published");

        let written = records(&path);
        assert!(
            !written.is_empty(),
            "a log that lost every event still says that it lost them"
        );
        let total: usize = written.iter().map(given).sum();
        assert_eq!(
            total, 3,
            "the window a line counts is the window the ring lost in, so the lines \
             add up to what a run gave up"
        );
        for record in &written {
            assert_eq!(
                record["level"].as_str(),
                Some("warn"),
                "a lost event is something an operator will want to know without a \
                 check having refused"
            );
            assert!(record["task_id"].is_null());
        }
    }

    #[test]
    fn a_gap_line_survives_a_threshold_that_hides_everything_else() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::with_capacity(0);
        let logger = log_at(&state, Level::Error, &bus);

        publish(
            &bus,
            None,
            EventKind::TaskQueued {
                title: "Hidden by the threshold".to_owned(),
            },
        );
        let path = logger.path().to_path_buf();
        logger
            .finish()
            .expect("finishing writes what the run published");

        let written = records(&path);
        assert_eq!(
            written.iter().map(given).sum::<usize>(),
            1,
            "the entry itself is `Info` and is filtered, and the line about what the \
             log cannot show is not"
        );
        for record in &written {
            assert_eq!(record["level"].as_str(), Some("warn"));
        }
    }

    #[test]
    fn finishing_writes_what_the_ring_holds_without_being_asked_to() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        publish(
            &bus,
            Some(task()),
            EventKind::TaskDone {
                commit: SHA.to_owned(),
            },
        );
        let path = logger.path().to_path_buf();
        logger
            .finish()
            .expect("finishing writes what the run published");

        let written = records(&path);
        assert_eq!(
            written.len(),
            1,
            "the log a run leaves behind holds every event the bus had published, \
             with no checkpoint asking for it"
        );
        assert_eq!(message(&written[0]), format!("TaskDone commit={SHA}"));
    }

    #[test]
    fn a_dropped_logger_still_leaves_the_lines_it_had_written() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);
        let path = logger.path().to_path_buf();

        publish(
            &bus,
            Some(task()),
            EventKind::VerifyPassed {
                attempt: AttemptId::new(4),
            },
        );
        drop(logger);

        let written = records(&path);
        assert_eq!(
            written.len(),
            1,
            "a run that unwound leaves the log it had reached rather than a file \
             that stopped mid-record"
        );
        assert_eq!(message(&written[0]), "VerifyPassed");
        assert_eq!(written[0]["attempt"].as_u64(), Some(4));
    }

    #[test]
    fn two_loggers_on_one_day_append_to_one_file() {
        let parent = scratch();
        let state = state_dir(&parent);
        let first_bus = Bus::new();
        let second_bus = Bus::new();

        let first = log_at(&state, Level::Info, &first_bus);
        publish(
            &first_bus,
            None,
            EventKind::TaskQueued {
                title: "Before the restart".to_owned(),
            },
        );
        first.flush().expect("a flush reaches the writer");

        let second = log_at(&state, Level::Info, &second_bus);
        assert_eq!(
            second.path(),
            first.path(),
            "one run crossing into a second logger keeps one file, so \"what \
             happened in this run\" has one answer"
        );
        publish(&second_bus, Some(task()), EventKind::Resumed);
        second.flush().expect("a flush reaches the writer");

        let written = records(first.path());
        assert_eq!(
            written.iter().map(message).collect::<Vec<&str>>(),
            ["TaskQueued title=Before the restart", "Resumed"],
            "both writers end whole records, so neither splits the other's line"
        );
        assert_eq!(
            mode(first.path()),
            LOG_FILE_MODE,
            "a file a second logger reopened for appending keeps the mode the first \
             opened it at"
        );
    }

    #[test]
    fn a_state_directory_that_is_not_there_is_refused_before_anything_is_written() {
        let parent = scratch();
        let state = parent.path().join("state");
        let refused = Logger::subscribe(&state, Level::Info, NO_EXTRA, &Bus::new())
            .expect_err("a log below a state directory that does not exist has nowhere to be");

        assert!(
            matches!(refused, Error::NotFound { .. }),
            "the refusal is the one that says where it looked: {refused}"
        );
        assert!(
            !state.exists(),
            "a refusal does not build the layout it refused to write below"
        );
    }

    #[test]
    fn a_state_directory_that_is_a_file_is_refused_by_name() {
        let parent = scratch();
        let state = parent.path().join("state");
        fs::write(&state, "not a directory").expect("a scratch file to refuse a log");

        let refused = Logger::subscribe(&state, Level::Info, NO_EXTRA, &Bus::new())
            .expect_err("a state directory that is a file cannot hold a log");

        match refused {
            Error::Policy { paths, .. } => assert_eq!(
                paths,
                vec![state.clone()],
                "the refusal names the path an operator has to fix, and no other"
            ),
            other => panic!("a file where a directory belongs is a policy refusal, not {other}"),
        }
        assert_eq!(
            raw(&state),
            "not a directory",
            "the refusal leaves the file it refused to write through alone"
        );
    }

    #[test]
    fn a_logs_directory_that_is_a_link_is_refused_rather_than_written_through() {
        let parent = scratch();
        let state = state_dir(&parent);
        let elsewhere = parent.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("a scratch directory for a link to point at");
        symlink(&elsewhere, state.join("logs")).expect("a scratch link where logs belong");

        let refused = Logger::subscribe(&state, Level::Info, NO_EXTRA, &Bus::new())
            .expect_err("a link where the log directory belongs is not a directory to write below");

        assert!(
            matches!(refused, Error::Policy { .. }),
            "a link is refused as an occupied path rather than followed: {refused}"
        );
        assert!(
            fs::read_dir(&elsewhere)
                .expect("the link's target is still there to be looked at")
                .next()
                .is_none(),
            "writing through the link would have put a run's log somewhere the \
             state directory does not own"
        );
    }

    #[test]
    fn an_entry_whose_payload_is_a_column_becomes_its_name_alone() {
        for kind in [
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
            EventKind::Interrupted {
                phase: Phase::Publish,
            },
            EventKind::PreflightStarted,
            EventKind::Resumed,
        ] {
            let name = kind.discriminant().to_owned();
            assert_eq!(
                message_of(&kind).expect("an entry with no facts to add has a spelling"),
                name,
                "the payload of `{name}` is the `attempt` and `phase` columns, so \
                 repeating it in the message is prose a filter cannot use"
            );
        }
    }

    #[test]
    fn a_message_names_the_facts_that_say_which_entry_it_was() {
        assert_eq!(
            message_of(&EventKind::TaskDone {
                commit: SHA.to_owned(),
            })
            .expect("a commit has a spelling"),
            format!("TaskDone commit={SHA}"),
            "the proof a task is done belongs in the line that says it is done"
        );
        assert_eq!(
            message_of(&EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "hello".to_owned(),
            })
            .expect("an output line has a spelling"),
            "AgentOutput stream=Stdout text=hello",
            "the stream is kept because the two are different questions, and the \
             attempt is not repeated because it is a column"
        );
        let acknowledged = message_of(&EventKind::GateAcknowledged {
            by: "ops@example.com".to_owned(),
            at: THEN,
        })
        .expect("an acknowledged gate has a spelling");
        assert_eq!(
            acknowledged,
            format!(
                "GateAcknowledged by=ops@example.com at={}",
                ts_text(THEN).expect("the instant every event is stamped with has a spelling")
            ),
            "an acknowledgement is auditable from the line alone, including when it \
             was made"
        );
    }

    #[test]
    fn a_gate_line_carries_its_verdict_not_its_output() {
        let refused = message_of(&EventKind::GateFinished {
            result: gate(false),
        })
        .expect("a gate's result has a spelling");
        assert!(
            refused.contains("GateFinished"),
            "the line says which entry it was: {refused}"
        );
        assert!(refused.contains("passed=false"), "{refused}");
        assert!(refused.contains("gate=verify"), "{refused}");
        assert!(
            !refused.contains("test result:"),
            "a gate's stdout lives in the journal, where the evidence is read from; \
             a log that copied it would be a second copy of the largest thing a run \
             writes: {refused}"
        );

        let passed = message_of(&EventKind::GateFinished { result: gate(true) })
            .expect("a gate's result has a spelling");
        assert!(passed.contains("passed=true"), "{passed}");
    }

    #[test]
    fn a_clock_set_before_the_epoch_keeps_its_side_of_the_epoch() {
        let before = instant_of(UNIX_EPOCH - Duration::from_secs(1))
            .expect("one second before the epoch is a date any calendar holds");
        assert_eq!(
            ts_text(before).expect("1969 has an RFC 3339 spelling"),
            "1969-12-31T23:59:59Z",
            "`duration_since` reports the distance from the epoch whichever way the \
             reading lies, so the sign is the part worth pinning"
        );
    }

    #[test]
    fn a_clock_no_calendar_can_hold_is_refused_rather_than_saturated() {
        let years_beyond_time = 12_000;
        let seconds_per_year = 31_557_600;
        let refused =
            instant_of(UNIX_EPOCH + Duration::from_secs(years_beyond_time * seconds_per_year))
                .expect_err("a reading no calendar can hold names no year to write");
        assert!(
            matches!(refused, Error::Serde(..)),
            "a clock reading with no spelling is a serialization failure, the same \
             refusal ADR-0012 gives the journal: {refused}"
        );
    }

    #[test]
    fn an_instant_authored_at_an_offset_is_written_in_utc() {
        let authored = datetime!(2026-09-21 11:14:03 +02:00);
        assert_eq!(
            ts_text(authored).expect("an instant two hours east of UTC has a spelling"),
            "2026-09-21T09:14:03Z",
            "one event has one moment in the log and the journal, and the journal's \
             spelling (ADR-0012) is UTC"
        );
    }

    #[test]
    fn an_instant_rfc_3339_has_no_digits_for_is_refused_rather_than_panicking() {
        let refused = ts_text(before_the_common_era())
            .expect_err("an instant in a negative year has no four digits to write");
        assert!(
            matches!(refused, Error::Serde(..)),
            "the refusal ADR-0012 chose over a panic: {refused}"
        );
    }

    #[test]
    fn a_record_that_cannot_be_written_is_reported_and_leaves_no_half_line() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        bus.publish(Event {
            seq: EventSeq::new(1),
            ts: before_the_common_era(),
            task_id: Some(task()),
            kind: EventKind::TaskDone {
                commit: SHA.to_owned(),
            },
        });
        let path = logger.path().to_path_buf();
        let refused = logger
            .finish()
            .expect_err("a record with no spelling cannot be written");

        assert!(
            matches!(refused, Error::Serde(..)),
            "the writer's failure reaches whoever asked for the log: {refused}"
        );
        assert_eq!(
            raw(&path),
            "",
            "a batch is written whole or not at all, so a reader never sees half of \
             a record"
        );
    }

    #[test]
    fn a_writer_that_is_no_longer_there_is_reported_not_assumed_finished() {
        let parent = scratch();
        let state = state_dir(&parent);
        let bus = Bus::new();
        let logger = log_at(&state, Level::Info, &bus);

        logger
            .close()
            .expect("the first close waits for the writer it reaped");
        let refused = logger
            .flush()
            .expect_err("nobody is left to answer a flush");

        assert!(
            matches!(refused, Error::Io(..)),
            "a log that stopped being written is an io-shaped refusal: {refused}"
        );
        assert!(
            refused.to_string().contains("may be incomplete"),
            "the refusal says what the operator has to conclude about the file: \
             {refused}"
        );
    }

    proptest! {
        #[test]
        fn arbitrary_message_text_stays_one_json_object_on_one_line(text in ".*") {
            let line = record_text(&Record {
                ts: "2026-09-21T09:14:03Z",
                level: Level::Debug.as_str(),
                task_id: Some(task()),
                attempt: Some(AttemptId::new(1)),
                phase: Some(Phase::Red),
                message: &text,
            })
            .expect("a record whose message is arbitrary still has a JSON encoding");
            prop_assert_eq!(line.matches('\n').count(), 1, "the message broke the one-line promise");
            let parsed: Value = serde_json::from_str(line.trim_end_matches('\n'))
                .expect("the line a record produced parses as the JSON it promises");
            let mut columns = parsed
                .as_object()
                .expect("a record is a JSON object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<&str>>();
            columns.sort_unstable();
            let mut documented = COLUMNS.to_vec();
            documented.sort_unstable();
            prop_assert_eq!(
                columns,
                documented,
                "a record holds exactly the six documented columns"
            );
            prop_assert_eq!(parsed["message"].as_str(), Some(text.as_str()));
        }
    }
}

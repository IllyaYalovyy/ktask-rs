//! The event catalog: the closed list of things that can be recorded as having
//! happened.
//!
//! A run *is* a sequence of these. The journal appends one before the side
//! effect it describes, `state::apply` folds the sequence into a `TaskState`,
//! the screens display them, and `--json` prints them — which makes this enum
//! the durable format rather than an internal detail. A renamed variant
//! silently changes data already on disk, and a field the document does not
//! list strands every journal written before it. So `docs/DESIGN.md` fixes the
//! catalog entry by entry, and the tests here pin each variant's name, each
//! payload field, and the refusal of any name the document does not use.
//!
//! Payloads are internally tagged (`#[serde(tag = "kind")]`), so one JSON
//! object carries both the answer to "what happened" and everything known
//! about it. The `events` table keeps both halves of that object: `kind` holds
//! [`EventKind::discriminant`] and `payload` holds the object itself. Two
//! sources for one string is the risk [`EventKind::discriminant`] exists to
//! keep honest, which is why a test compares it against the tag on every
//! variant rather than on one.
//!
//! # The envelope
//!
//! [`Event`] is the record around a catalog entry: sequence, instant, task and
//! payload — the four columns `docs/DESIGN.md` gives the `events` table. A
//! catalog entry on its own says *what* happened to no one at no time, so a
//! journal of them could not be replayed in order, timed, or attributed to a
//! task; the envelope is what makes one readable as a record of a run.
//!
//! Its timestamp is written as RFC 3339 in UTC by [`rfc3339_utc`], the
//! "Time is `OffsetDateTime` in UTC, serialized as RFC 3339" convention of
//! `docs/DESIGN.md` made mechanical rather than intended.
//!
//! # Catalog entries that are not here yet
//!
//! `docs/DESIGN.md` lists 28 entries; 26 are defined below. The other 2 are
//! absent, and a test asserts their absence rather than trusting it:
//!
//! - Two are deferred by the plan — `ProviderDetected`
//!   (`capabilities: Capabilities`) and `DecisionResolved`. Each arrives with
//!   the task that emits it and gives it an `apply` arm, so nothing can journal
//!   an event whose effect on state no task has written yet.
//!   `AttemptRecorded` left this list for T068, `AttemptFinished` for T091, which
//!   runs the session whose end it records,
//!   `TddExceptionUsed` for T079, which wrote the arm §9's exception is
//!   answered by, `DecisionRaised` for T084, which wrote the arm §6's wait for a
//!   decision is answered by, and `SelfHealingReport` for T095, which files the
//!   account §7 says every recovery leaves and wrote the arm that state answers
//!   it with.
//! - None is deferred for a missing payload type any more. `GateStarted` and
//!   `GateFinished` were the two entries waiting on `gate.rs`, and both landed
//!   with T085, the task that emits them and writes their `apply` arms.
//!   `GateStarted`'s payload is `gate: GateKind` rather than the `kind`
//!   `docs/DESIGN.md` first spelled it: the tag owns that key, so the entry was
//!   unwritable until its field was renamed (ADR-0011 measured it, ADR-0080
//!   records the rename and the document correction).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::attempt::AttemptRecord;
use crate::classify::{FailureClass, TddException};
use crate::decision::DecisionRequest;
use crate::gate::{GateKind, GateResult};
use crate::ids::{AttemptId, EventSeq, TaskId};
use crate::provider::Usage;
use crate::state::{PauseReason, Phase, Recovery, Stream};

/// Something that happened — to the queue, to a task, or to one run of a task.
///
/// Every field is a fact observed at the moment of the event, not a view of
/// current state: the journal is append-only and never rewritten, so what a
/// run looked like halfway through stays readable after it finished.
///
/// It is `PartialEq` rather than [`Eq`], and [`Event`] follows it: an attempt's
/// record carries the cost its provider reported, which is a float, and a float
/// is not an equivalence relation. The cost stays a float rather than being
/// rounded into microdollars, so the number in the journal is the number the
/// provider said (ADR-0063).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EventKind {
    /// A task was added to the queue.
    TaskQueued {
        /// The title, projected from the task's own first line.
        title: String,
    },
    /// Preflight has begun: the checks that decide whether work may start.
    PreflightStarted,
    /// Preflight passed, and the commit the work will be based on is known.
    PreflightPassed {
        /// The commit every later commit will be checked against.
        base_sha: String,
    },
    /// Preflight refused the task, so no attempt was started.
    PreflightFailed {
        /// Which kind of refusal this is — the only part a response is chosen
        /// from.
        class: FailureClass,
        /// What was found, in the words of the check that found it.
        detail: String,
    },
    /// An agent process was started, and this is the record that proves it was.
    AttemptStarted {
        /// Which run of the task this is.
        attempt: AttemptId,
        /// The work protocol in force for this attempt.
        protocol: String,
        /// The agent's process id, so recovery can tell a dead run from a live
        /// one instead of asking it.
        pid: u32,
        /// The commit the attempt started from.
        base_sha: String,
    },
    /// The attempt moved into a protocol phase.
    PhaseEntered {
        /// Which run of the task entered it.
        attempt: AttemptId,
        /// The phase now in progress.
        phase: Phase,
    },
    /// One line of agent output, kept from the moment it was read.
    AgentOutput {
        /// Which run of the task produced it.
        attempt: AttemptId,
        /// Which of the agent's two streams it arrived on; the distinction is
        /// unrecoverable once the two are merged.
        stream: Stream,
        /// The line, after secret redaction.
        text: String,
    },
    /// One agent session stopped, and these are the five things only it knew.
    ///
    /// How it stopped, what it spent, which session it was, and which model it
    /// said it ran on: each is a fact the session carried to its last moment and
    /// nobody else was holding, which is why the row is written when the session
    /// ends rather than assembled afterwards from what the run happened to keep.
    ///
    /// It moves a task nowhere, like [`EventKind::AttemptRecorded`], and for the
    /// same reason plus one more: §3's invariant 4 forbids a task being done on an
    /// agent's exit code or statement, so the way out of `running` is a phase's
    /// gate and not a session's exit. [`crate::apply`] therefore answers it with
    /// the state it was asked from, in every state that holds the attempt it names
    /// — ADR-0086 records those four, and why the entry that ends an attempt is not
    /// the entry that closes a task.
    ///
    /// Nor is it the attempt's record, which [`EventKind::AttemptRecorded`] files
    /// as one row per attempt with its gates, its SHAs and its task beside them.
    /// These are the five fields `docs/DESIGN.md` lists for this entry and nothing
    /// else, so a retry of a task adds a session's account instead of rewriting an
    /// attempt's evidence, and ADR-0057 is why [`Self::AttemptFinished::model_reported`]
    /// stays `None` rather than being filled in with the configured id.
    AttemptFinished {
        /// Which run of the task ended. It is the row's attribution, and the reason
        /// a state holding another attempt refuses it.
        attempt: AttemptId,
        /// The status the session's own process stopped with. It is evidence of how
        /// the session ended and nothing more: a scenario is free to contradict its
        /// own report with this number, which is what invariant 4 looks like in a
        /// test.
        exit_code: i32,
        /// What the session said it spent, or `None` when nobody asked it. No
        /// figure is written where an unmeasured one belongs (ADR-0049).
        usage: Option<Usage>,
        /// The session's own identifier, when it disclosed one. No correctness path
        /// depends on resuming a session (VISION.md §12), so an adapter that never
        /// reveals one has cost nothing.
        session_id: Option<String>,
        /// The model the session said it ran on, or `None` when it said nothing.
        /// [`check_model`](crate::provider::check_model) compares this with the
        /// configured id *before* this row is written, so a mismatch is refused as
        /// a configuration failure and what reaches the journal is a confirmed id
        /// or an honest absence.
        model_reported: Option<String>,
    },
    /// One mechanical gate began running: the runner's own command, not an
    /// agent's claim about it.
    GateStarted {
        /// Which gate it was, since a gate is identified by its kind and not by
        /// its command.
        ///
        /// `gate` rather than the `kind` `docs/DESIGN.md` first spelled it,
        /// because `kind` is the key the payload's own tag already owns
        /// (ADR-0080).
        gate: GateKind,
    },
    /// One mechanical gate finished, and this is what its run produced.
    GateFinished {
        /// The record the run left behind: verdict, status, output. The gate's
        /// kind is inside it, which is why this payload nests rather than
        /// naming the kind beside the verdict (ADR-0036).
        result: GateResult,
    },
    /// The mandatory verification gates passed.
    VerifyPassed {
        /// Which run of the task passed.
        attempt: AttemptId,
    },
    /// The mandatory verification gates failed.
    VerifyFailed {
        /// Which run of the task failed.
        attempt: AttemptId,
        /// Which class of failure the gates produced.
        class: FailureClass,
        /// The gate output that says so.
        detail: String,
    },
    /// A commit exists locally and is on its way to the remote.
    PublishStarted {
        /// Which run of the task is being published.
        attempt: AttemptId,
        /// The commit being pushed.
        candidate_sha: String,
    },
    /// The remote was read back and holds the commit.
    PublishVerified {
        /// The commit that was pushed.
        commit: String,
        /// The remote's own tip, which must name the same commit — the
        /// difference between "we pushed" and "the remote has it".
        remote_sha: String,
    },
    /// The task is done, and the commit that proves it is named beside it.
    TaskDone {
        /// The published commit.
        commit: String,
    },
    /// The task will not get done, in a form a response can be chosen from.
    TaskFailed {
        /// What kind of failure ended it.
        class: FailureClass,
        /// What was observed, in the words of whoever observed it.
        detail: String,
    },
    /// An operator stopped the task before it finished.
    TaskCancelled {
        /// Why, as the operator gave it.
        reason: String,
    },
    /// The run is standing still, waiting for something.
    Paused {
        /// What it is waiting for, which is what decides what resumes it.
        reason: PauseReason,
    },
    /// A paused run is running again.
    Resumed,
    /// The supervisor died or was signalled mid-phase.
    Interrupted {
        /// The phase the journal ended in, which is where recovery looks
        /// first.
        phase: Phase,
    },
    /// Recovery decided what to do with an interrupted run.
    RecoveryDecision {
        /// What recovery concluded.
        decision: Recovery,
        /// The evidence it concluded it from.
        detail: String,
    },
    /// A task used a declared exception to test-first, and did not write its
    /// tests first.
    ///
    /// The record VISION.md §9 exists to make possible. Test-first order cannot
    /// be proved after the fact, so a task that genuinely does not fit applies
    /// for one of the four categories instead of writing a failing test — and
    /// the override is worth nothing unless the run says which category it used
    /// and why, in the journal, beside the attempt that skipped the red phase.
    /// ADR-0010 records where the four categories came from; ADR-0074 records
    /// why one type spells them.
    ///
    /// It moves a task nowhere: like [`EventKind::AttemptRecorded`] it says what
    /// was, and [`crate::apply`] answers it with the state it was asked from —
    /// refused everywhere but where an agent is working, which is what makes §9's
    /// exception unusable as a route around a scope violation.
    TddExceptionUsed {
        /// Which of §9's four categories was claimed.
        exception: TddException,
        /// Why its author said it applied, kept because an override nobody can
        /// audit is an override nobody will admit to having used.
        reason: String,
    },
    /// An agent stopped at a decision it is not authorised to make, and asked a
    /// human to make it.
    ///
    /// The ask itself is the payload, because VISION.md §6 makes a wait for input
    /// carry "a structured decision request (question, options, trade-offs,
    /// impact)" — the parts a decision is actually answered from, read off the
    /// report by [`decision_request`](crate::decision_request) and stored whole,
    /// so the inbox shows a human what was asked rather than that something was.
    /// ADR-0079 records why a report that asked nothing raises no event at all,
    /// and so why this payload cannot be empty.
    DecisionRaised {
        /// What was asked, in the parts a decision is made from.
        request: DecisionRequest,
    },
    /// A human acknowledged a gate that had stopped the run.
    GateAcknowledged {
        /// Who acknowledged it.
        by: String,
        /// When, so an acknowledgement can be audited after the fact rather
        /// than taken on trust.
        at: OffsetDateTime,
    },
    /// One attempt finished, and this is everything it proved.
    ///
    /// The record is the whole of an attempt's evidence — its session, the two
    /// model ids, how it stopped, what every gate said, what it spent, and the
    /// commits it sits between — filed as one row rather than scattered over
    /// the entries that happened to be journaled along the way. It changes
    /// nothing about where the task stands: [`crate::apply`] answers it with
    /// the state it was asked from, because an attempt's evidence says what
    /// was, not what happens next. A retry therefore *adds* a record; the
    /// first attempt's row keeps what it knew (VISION.md §6).
    ///
    /// The record is held behind a [`Box`] for the same reason
    /// [`crate::TaskState::Paused`] holds its `resume_to` behind one: this is
    /// the largest payload in the catalog, and unboxed every other entry would
    /// be sized by it. The box is invisible in the bytes — `serde` writes a box
    /// exactly as it writes what it holds — so a journal row stays the
    /// `record: AttemptRecord` `docs/DESIGN.md` spells (ADR-0063).
    AttemptRecorded {
        /// The attempt's own evidence, naming the attempt and task it belongs
        /// to.
        record: Box<AttemptRecord>,
    },
    /// One remediation's account of itself: the failure it was aimed at, the
    /// repairs it tried, and what it ended with.
    ///
    /// VISION.md §7 requires that "every recovery produces a self-healing
    /// report: classification, attempted repairs, final result", and this is
    /// that record. It is the third kind of catalog entry — evidence rather
    /// than a transition — so [`crate::apply`] answers it with the state it was
    /// asked from, and a task that was remediated twice leaves two of them
    /// while one remediated once leaves exactly one, whichever way the
    /// remediation ended. T095 defines it, files it in the attempt's own
    /// evidence directory, and gives it the one `apply` arm §7's machine allows.
    ///
    /// The classification is carried here rather than read back off the failure
    /// the journal already holds: an account of a repair has to name the
    /// failure it set out to answer, or a reader cannot tell whether the two
    /// records are about the same problem or about two different ones that
    /// happened to land on the same task.
    SelfHealingReport {
        /// The remediation attempt this is the account of.
        attempt: AttemptId,
        /// The class the remediation was launched against.
        class: FailureClass,
        /// The repairs it attempted, in the order it attempted them. Empty is
        /// an answer: a recovery that was bound before it could try anything
        /// tried nothing.
        repairs: Vec<String>,
        /// What it ended with, in the words of the step that ended it.
        outcome: String,
    },
}

impl EventKind {
    /// The name of this variant, as the journal's `kind` column stores it.
    ///
    /// The same string `#[serde(tag = "kind")]` writes into the payload: the
    /// column is what an index and a query read without parsing JSON, the tag
    /// is what a re-read payload carries. Keeping them equal is what makes
    /// `SELECT kind = 'TaskDone'` and the decoded enum agree.
    #[must_use]
    pub fn discriminant(&self) -> &'static str {
        match self {
            Self::TaskQueued { .. } => "TaskQueued",
            Self::PreflightStarted { .. } => "PreflightStarted",
            Self::PreflightPassed { .. } => "PreflightPassed",
            Self::PreflightFailed { .. } => "PreflightFailed",
            Self::AttemptStarted { .. } => "AttemptStarted",
            Self::PhaseEntered { .. } => "PhaseEntered",
            Self::AgentOutput { .. } => "AgentOutput",
            Self::AttemptFinished { .. } => "AttemptFinished",
            Self::GateStarted { .. } => "GateStarted",
            Self::GateFinished { .. } => "GateFinished",
            Self::VerifyPassed { .. } => "VerifyPassed",
            Self::VerifyFailed { .. } => "VerifyFailed",
            Self::PublishStarted { .. } => "PublishStarted",
            Self::PublishVerified { .. } => "PublishVerified",
            Self::TaskDone { .. } => "TaskDone",
            Self::TaskFailed { .. } => "TaskFailed",
            Self::TaskCancelled { .. } => "TaskCancelled",
            Self::Paused { .. } => "Paused",
            Self::Resumed { .. } => "Resumed",
            Self::Interrupted { .. } => "Interrupted",
            Self::RecoveryDecision { .. } => "RecoveryDecision",
            Self::TddExceptionUsed { .. } => "TddExceptionUsed",
            Self::DecisionRaised { .. } => "DecisionRaised",
            Self::GateAcknowledged { .. } => "GateAcknowledged",
            Self::AttemptRecorded { .. } => "AttemptRecorded",
            Self::SelfHealingReport { .. } => "SelfHealingReport",
        }
    }
}

/// One journal record: what happened, and the three facts that place it in a
/// run.
///
/// The four fields are the four columns `docs/DESIGN.md` gives the `events`
/// table. [`EventKind`] alone says what happened to no one at no time: with no
/// `seq` a replay has no order to fold the events in, with no `ts` the History
/// screen is a list without dates, and with no `task_id` a queue-level event
/// and a task's own event are indistinguishable. Read back in `seq` order, a
/// sequence of these *is* the run — which is why the journal, the event bus and
/// `--json` all hand round exactly this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Where this record sits in the journal's sequence. Assigned by the
    /// journal as it appends, never guessed by whoever produced the event: a
    /// sequence a caller chose is a sequence two callers can choose.
    pub seq: EventSeq,
    /// When it happened, written as RFC 3339 in UTC: one instant, one
    /// spelling, whatever offset it was stamped at.
    #[serde(with = "rfc3339_utc")]
    pub ts: OffsetDateTime,
    /// The task this event is about, or `None` for an event about the queue
    /// itself — the `NULL` of the `events` table's `task_id` column.
    pub task_id: Option<TaskId>,
    /// What happened, with everything known about it.
    ///
    /// Encoded as the internally-tagged object [`EventKind`] derives, so the
    /// object inside an envelope and the object the `payload` column holds are
    /// the same bytes: a stored event and a streamed one read identically.
    pub kind: EventKind,
}

/// Serde glue that writes an instant as RFC 3339 text and reads it back in UTC.
///
/// [`Event`]'s field asks for this module rather than for `time`'s own
/// `serde`-feature encoding, because that encoding — with the `time` features
/// `docs/DESIGN.md` fixes, which do not include `serde-human-readable` — is a
/// numeric tuple no operator can read, and widening the dependency set is not
/// this module's call to make. Formatting and parsing are still `time`'s
/// (`time::serde::rfc3339`); what is added here is the direction. An instant
/// authored at `+02:00` and one authored at `Z` are the same instant, and the
/// `ts` column documents one spelling of it, so the offset is resolved before
/// the text is written: whatever offset an event was stamped at, the journal
/// holds one spelling of it.
///
/// Writing needs the conversion; reading does not. Measured here rather than
/// assumed: `time`'s RFC 3339 parser hands back the instant it read at the UTC
/// offset whatever offset the text carried, so the read side forwards and a
/// test holds that promise — `time`'s writer, by contrast, keeps the offset the
/// instant arrived with, which is why the write side converts.
///
/// Writing is total. `time`'s own conversion (`to_offset`) panics once the
/// result leaves the supported date range, and a supervisor that panics loses
/// the run it was supervising, so the checked form is used and its refusal
/// becomes a serialization failure: an instant an hour past the last representable
/// date has no UTC spelling to write. `time`'s formatter supplies the other
/// half, refusing a year RFC 3339 has no digits for.
mod rfc3339_utc {
    use serde::ser::Error as _;
    use serde::{Deserializer, Serializer};
    use time::{OffsetDateTime, UtcOffset};

    /// Write `stamp` as RFC 3339 text, in UTC.
    pub(crate) fn serialize<S>(stamp: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let utc = stamp
            .checked_to_offset(UtcOffset::UTC)
            .ok_or_else(|| S::Error::custom("instant has no UTC calendar date to write"))?;
        time::serde::rfc3339::serialize(&utc, serializer)
    }

    /// Read RFC 3339 text and return the instant it names, which `time`
    /// returns at the UTC offset.
    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        time::serde::rfc3339::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, EventKind};
    use crate::attempt::AttemptRecord;
    use crate::classify::{FailureClass, TddException};
    use crate::decision::DecisionRequest;
    use crate::gate::{GateKind, GateResult};
    use crate::ids::{AttemptId, EventSeq, TaskId};
    use crate::provider::{Usage, UsageSource};
    use crate::state::{PauseReason, Phase, Recovery, Stream};
    use serde_json::Value;
    use time::macros::datetime;
    use time::{OffsetDateTime, UtcOffset};

    /// A commit sha every entry below is built from, so a field that fails to
    /// encode is visible rather than mistaken for a placeholder.
    const SHA: &str = "0b78d3f1c2a4";

    /// The 26 entries `docs/DESIGN.md` documents whose payload types exist
    /// today, each with the payload field names the table lists for it.
    ///
    /// Spelled out a second time, on purpose: names checked only against the
    /// code pass by agreeing with themselves, which is the false confidence a
    /// durable format cannot afford.
    const DOCUMENTED: &[(&str, &[&str])] = &[
        ("TaskQueued", &["title"]),
        ("PreflightStarted", &[]),
        ("PreflightPassed", &["base_sha"]),
        ("PreflightFailed", &["class", "detail"]),
        (
            "AttemptStarted",
            &["attempt", "protocol", "pid", "base_sha"],
        ),
        ("PhaseEntered", &["attempt", "phase"]),
        ("AgentOutput", &["attempt", "stream", "text"]),
        (
            "AttemptFinished",
            &[
                "attempt",
                "exit_code",
                "usage",
                "session_id",
                "model_reported",
            ],
        ),
        ("GateStarted", &["gate"]),
        ("GateFinished", &["result"]),
        ("VerifyPassed", &["attempt"]),
        ("VerifyFailed", &["attempt", "class", "detail"]),
        ("PublishStarted", &["attempt", "candidate_sha"]),
        ("PublishVerified", &["commit", "remote_sha"]),
        ("TaskDone", &["commit"]),
        ("TaskFailed", &["class", "detail"]),
        ("TaskCancelled", &["reason"]),
        ("Paused", &["reason"]),
        ("Resumed", &[]),
        ("Interrupted", &["phase"]),
        ("RecoveryDecision", &["decision", "detail"]),
        ("TddExceptionUsed", &["exception", "reason"]),
        ("DecisionRaised", &["request"]),
        ("GateAcknowledged", &["by", "at"]),
        ("AttemptRecorded", &["record"]),
        (
            "SelfHealingReport",
            &["attempt", "class", "repairs", "outcome"],
        ),
    ];

    /// The two entries this catalog does not define yet.
    ///
    /// Their absence is asserted, not assumed: an entry added ahead of its
    /// producer would start decoding, and the journal would begin accepting
    /// events the plan says nothing may emit until the task that writes that
    /// producer lands.
    ///
    /// The second half of each pair is the payload `docs/DESIGN.md` documents
    /// for the entry, minus the tag the test adds. The populated payload is
    /// what makes the refusal an assertion: `{"kind":"DecisionResolved"}` is
    /// refused for its missing fields whether or not the entry exists, so a
    /// bare name would prove nothing either way.
    const DEFERRED_PAYLOADS: &[(&str, &str)] = &[
        (
            "ProviderDetected",
            r#""provider":"codex","capabilities":{},"version":"0.1.0""#,
        ),
        (
            "DecisionResolved",
            r#""adr_path":"docs/adr/0011.md","answer":"a""#,
        ),
    ];

    /// One instance of every documented entry, in [`DOCUMENTED`]'s order.
    fn one_event_per_documented_entry() -> Vec<EventKind> {
        let attempt = AttemptId::new(2);
        vec![
            EventKind::TaskQueued {
                title: "Define the event catalog".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: SHA.to_string(),
            },
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "git is missing".to_string(),
            },
            EventKind::AttemptStarted {
                attempt,
                protocol: "tdd".to_string(),
                pid: 4242,
                base_sha: SHA.to_string(),
            },
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Red,
            },
            EventKind::AgentOutput {
                attempt,
                stream: Stream::Stderr,
                text: "panicked at 'index out of bounds'".to_string(),
            },
            EventKind::AttemptFinished {
                attempt,
                exit_code: 0,
                usage: Some(Usage {
                    input_tokens: Some(8_120),
                    output_tokens: Some(1_944),
                    cached_tokens: Some(6_400),
                    cost_usd: Some(0.42),
                    source: UsageSource::Provider,
                }),
                session_id: Some("sess_01HQZK".to_string()),
                model_reported: Some("gpt-5.6-sol".to_string()),
            },
            EventKind::GateStarted {
                gate: GateKind::Lint,
            },
            EventKind::GateFinished {
                result: gate_result(GateKind::Lint),
            },
            EventKind::VerifyPassed { attempt },
            EventKind::VerifyFailed {
                attempt,
                class: FailureClass::VerificationFailure,
                detail: "gate targeted failed".to_string(),
            },
            EventKind::PublishStarted {
                attempt,
                candidate_sha: SHA.to_string(),
            },
            EventKind::PublishVerified {
                commit: SHA.to_string(),
                remote_sha: SHA.to_string(),
            },
            EventKind::TaskDone {
                commit: SHA.to_string(),
            },
            EventKind::TaskFailed {
                class: FailureClass::GitConflict,
                detail: "push rejected: non-fast-forward".to_string(),
            },
            EventKind::TaskCancelled {
                reason: "operator withdrew the task".to_string(),
            },
            EventKind::Paused {
                reason: PauseReason::Input,
            },
            EventKind::Resumed,
            EventKind::Interrupted {
                phase: Phase::Publish,
            },
            EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "journal ends mid-phase; nothing was published".to_string(),
            },
            EventKind::TddExceptionUsed {
                exception: TddException::Documentation,
                reason: "documentation only; no behaviour to pin".to_string(),
            },
            EventKind::DecisionRaised {
                request: DecisionRequest {
                    question: "Which layout does the journal keep?".to_string(),
                    options: vec!["sequence".to_string(), "rowid".to_string()],
                    tradeoffs: "a raw dump stays readable".to_string(),
                    impact: "every replay".to_string(),
                    recommended: Some("sequence".to_string()),
                },
            },
            EventKind::GateAcknowledged {
                by: "operators.name".to_string(),
                at: datetime!(2026-09-17 12:34:56 UTC),
            },
            EventKind::AttemptRecorded {
                record: Box::new(attempt_record(attempt)),
            },
            EventKind::SelfHealingReport {
                attempt,
                class: FailureClass::VerificationFailure,
                repairs: vec!["re-run the fmt gate".to_string()],
                outcome: "green on the rerun".to_string(),
            },
        ]
    }

    /// One attempt's evidence, holding what the record's own module fixes. The
    /// gates list is empty here on purpose: what a nested [`AttemptRecord`]
    /// keeps field by field is `attempt.rs`'s claim to test, and this file's is
    /// that the catalog carries the record as one payload field.
    fn attempt_record(attempt: AttemptId) -> AttemptRecord {
        AttemptRecord {
            id: attempt,
            task: TaskId::new(7),
            started: datetime!(2026-09-20 09:14:03.5 UTC),
            ended: Some(datetime!(2026-09-20 09:41:47 UTC)),
            model_configured: Some("gpt-5.6-sol".to_string()),
            model_reported: Some("gpt-5.6-sol-2026-09-01".to_string()),
            session_id: Some("sess_01HQZK".to_string()),
            exit_reason: "gate verify failed: 2 tests refused".to_string(),
            gates: Vec::new(),
            usage: None,
            base_sha: SHA.to_string(),
            candidate_sha: Some("b7d1f3a9e5c2".to_string()),
        }
    }

    /// One gate's record, as a run that refused would leave it. Both halves of
    /// the exit status are exercised by the catalog's own round trip: a status
    /// the command returned and no signal, which is the pair a refusal that
    /// outlived its budget never makes.
    fn gate_result(kind: GateKind) -> GateResult {
        GateResult {
            kind,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 4_812,
            stdout: String::new(),
            stderr: "clippy::redundant_clone: 2 found\n".to_string(),
            timed_out: false,
        }
    }

    /// Assert the JSON the journal stores for `event` says `name`, carries
    /// exactly `fields`, and decodes back into the same event.
    fn encodes_as(name: &str, fields: &[&str], event: &EventKind) {
        let encoded = serde_json::to_value(event).expect("a catalog entry encodes as JSON");
        let object = encoded.as_object().expect("an event is a JSON object");
        assert_eq!(
            object.get("kind"),
            Some(&Value::String(name.to_string())),
            "{name} must be tagged with its own variant name"
        );
        let mut carried: Vec<&str> = object
            .keys()
            .filter(|key| key.as_str() != "kind")
            .map(String::as_str)
            .collect();
        let mut documented = fields.to_vec();
        carried.sort_unstable();
        documented.sort_unstable();
        assert_eq!(
            carried, documented,
            "{name} must carry exactly the payload fields docs/DESIGN.md lists, and no others"
        );

        let text = serde_json::to_string(event).expect("a catalog entry encodes as a string");
        let decoded: EventKind =
            serde_json::from_str(&text).expect("what the catalog writes is read back");
        assert_eq!(
            &decoded, event,
            "{name} must survive the round trip unchanged"
        );
    }

    #[test]
    fn events_round_trip_through_json_with_their_documented_fields() {
        let events = one_event_per_documented_entry();
        assert_eq!(
            events.len(),
            DOCUMENTED.len(),
            "one instance per documented entry is what makes this a check rather than a sample"
        );
        for ((name, fields), event) in DOCUMENTED.iter().zip(&events) {
            encodes_as(name, fields, event);
        }
    }

    #[test]
    fn event_discriminant_is_the_variant_name_the_journal_indexes_on() {
        let events = one_event_per_documented_entry();
        for ((name, _), event) in DOCUMENTED.iter().zip(&events) {
            assert_eq!(
                event.discriminant(),
                *name,
                "the kind column must hold the variant name itself, not another spelling of it"
            );
            let encoded = serde_json::to_value(event).expect("a catalog entry encodes as JSON");
            assert_eq!(
                encoded.get("kind").and_then(Value::as_str),
                Some(event.discriminant()),
                "{name}: the stored kind and the payload tag must be the same string",
            );
        }
    }

    #[test]
    fn event_kind_refuses_a_name_the_catalog_does_not_define() {
        for name in ["Unsupported", "taskDone", "Task_Queued", "TaskQueue", ""] {
            let rejected = format!(r#"{{"kind":"{name}"}}"#);
            assert!(
                serde_json::from_str::<EventKind>(&rejected).is_err(),
                "{name} is not in the catalog and must not decode as if it were",
            );
        }
    }

    #[test]
    fn event_kind_refuses_a_deferred_entry_even_fully_populated() {
        for (name, fields) in DEFERRED_PAYLOADS {
            let payload = format!(r#"{{"kind":"{name}",{fields}}}"#);
            let _: Value = serde_json::from_str(&payload)
                .expect("the sample payload must parse, or the refusal below proves nothing");
            assert!(
                serde_json::from_str::<EventKind>(&payload).is_err(),
                "{name} has no entry in the catalog yet, so {payload} must be \
                 refused rather than silently dropped",
            );
        }
    }

    #[test]
    fn event_kind_refuses_a_payload_that_is_not_the_documented_shape() {
        for payload in [
            r#"{"kind":"TaskQueued"}"#,
            r#"{"kind":"TaskDone","commit":7}"#,
            concat!(
                r#"{"kind":"AttemptStarted","attempt":2,"protocol":"tdd","#,
                r#""pid":"4242","base_sha":"0b78d3f1c2a4"}"#,
            ),
            r#"{"title":"no kind at all"}"#,
        ] {
            assert!(
                serde_json::from_str::<EventKind>(payload).is_err(),
                "{payload} is not a shape the catalog documents",
            );
        }
    }

    #[test]
    fn event_without_a_payload_encodes_as_the_tag_alone() {
        for event in [EventKind::PreflightStarted, EventKind::Resumed] {
            let name = event.discriminant();
            let encoded = serde_json::to_string(&event).expect("a payload-less event encodes");
            assert_eq!(
                encoded,
                format!(r#"{{"kind":"{name}"}}"#),
                "a payload-less event must encode as its kind alone, or a replay \
                 reads back a fact nobody recorded"
            );
        }
    }

    #[test]
    fn event_keeps_numbers_as_numbers_and_the_instant_as_an_instant() {
        let events = one_event_per_documented_entry();
        let started = events
            .iter()
            .find(|candidate| candidate.discriminant() == "AttemptStarted")
            .expect("the catalog holds an AttemptStarted");
        let encoded = serde_json::to_value(started).expect("a catalog entry encodes as JSON");
        assert_eq!(
            encoded.get("pid").and_then(Value::as_u64),
            Some(4242),
            "a pid is a number the journal can be queried by, not a string that has to be parsed",
        );
        assert_eq!(
            encoded.get("attempt").and_then(Value::as_u64),
            Some(2),
            "an AttemptId must keep the number it wraps, as it does in ids.rs",
        );

        let acknowledged = events
            .iter()
            .find(|candidate| candidate.discriminant() == "GateAcknowledged")
            .expect("the catalog holds a GateAcknowledged");
        let text =
            serde_json::to_string(acknowledged).expect("a catalog entry encodes as a string");
        let EventKind::GateAcknowledged { by, at } =
            serde_json::from_str::<EventKind>(&text).expect("an acknowledgement is read back")
        else {
            panic!("an acknowledgement must decode as a GateAcknowledged");
        };
        assert_eq!(by, "operators.name");
        assert_eq!(
            at.unix_timestamp(),
            datetime!(2026-09-17 12:34:56 UTC).unix_timestamp(),
            "an acknowledgement is only auditable if the instant survives the journal",
        );
    }

    /// The four columns `docs/DESIGN.md` gives the `events` table, with `seq`
    /// taken as given and `kind` left to the caller.
    fn envelope(seq: u64, ts: OffsetDateTime, task_id: Option<TaskId>, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(seq),
            ts,
            task_id,
            kind,
        }
    }

    #[test]
    fn envelope_writes_the_instant_as_rfc_3339_in_utc() {
        let event = envelope(
            7,
            datetime!(2026-09-17 12:34:56 UTC),
            Some(TaskId::new(3)),
            EventKind::TaskDone {
                commit: SHA.to_string(),
            },
        );
        assert_eq!(
            serde_json::to_string(&event).expect("an envelope encodes"),
            concat!(
                r#"{"seq":7,"ts":"2026-09-17T12:34:56Z","#,
                r#""task_id":3,"kind":{"kind":"TaskDone","commit":"0b78d3f1c2a4"}}"#,
            ),
            "the envelope's text is the durable format: the four columns the events \
             table has, and the instant in the Z form its column comment promises",
        );
    }

    #[test]
    fn envelope_writes_one_instant_as_one_text_whatever_offset_it_arrived_with() {
        for (authored, expected) in [
            (datetime!(2026-09-17 12:34:56 UTC), "2026-09-17T12:34:56Z"),
            (
                datetime!(2026-09-17 14:34:56 +02:00),
                "2026-09-17T12:34:56Z",
            ),
            (
                datetime!(2026-09-17 05:34:56 -07:00),
                "2026-09-17T12:34:56Z",
            ),
        ] {
            let event = envelope(1, authored, None, EventKind::Resumed);
            let encoded = serde_json::to_value(&event).expect("an envelope encodes as JSON");
            assert_eq!(
                encoded.get("ts").and_then(Value::as_str),
                Some(expected),
                "{authored} is the same instant as {expected}; the ts column holds one \
                 spelling of an instant, not the spelling whoever stamped it happened to use",
            );
        }
    }

    #[test]
    fn envelope_reads_the_instant_back_as_the_same_instant() {
        let event = envelope(
            9,
            datetime!(2026-09-17 12:34:56.123456789 UTC),
            Some(TaskId::new(4)),
            EventKind::TaskQueued {
                title: "Define the event envelope".to_string(),
            },
        );
        let text = serde_json::to_string(&event).expect("an envelope encodes as a string");
        assert!(
            text.contains(r#""ts":"2026-09-17T12:34:56.123456789Z""#),
            "two events stamped inside one second are only ordered by their fraction of \
             it, and the text written was {text}",
        );

        let decoded: Event =
            serde_json::from_str(&text).expect("what the envelope writes is read back");
        assert_eq!(
            decoded.ts, event.ts,
            "parsing the text must yield the instant it was written from"
        );
        assert_eq!(
            decoded.ts.offset(),
            UtcOffset::UTC,
            "a read-back instant must carry the offset it was written with, or the next \
             write of it differs from the last"
        );
        assert_eq!(decoded, event, "the whole record, not only its timestamp");
        assert_eq!(
            serde_json::to_string(&decoded).expect("a read-back envelope re-encodes"),
            text,
            "a replay that re-writes a record must write the bytes it read, or the journal \
             is rewritten by being read"
        );
    }

    #[test]
    fn envelope_canonicalizes_the_spellings_rfc_3339_allows_by_agreement() {
        // An offset in place of `Z`, and a space in place of the `T`, are both
        // RFC 3339 by mutual agreement. Both name a real instant, so both are
        // readable; both leave with one spelling, so a re-write is not free to
        // keep the spelling its author happened to use.
        for text in [
            concat!(
                r#"{"seq":1,"ts":"2026-09-17T14:34:56+02:00","#,
                r#""task_id":null,"kind":{"kind":"Resumed"}}"#,
            ),
            concat!(
                r#"{"seq":1,"ts":"2026-09-17 12:34:56Z","#,
                r#""task_id":null,"kind":{"kind":"Resumed"}}"#,
            ),
        ] {
            let decoded: Event = serde_json::from_str(text)
                .expect("RFC 3339 text that names an instant is readable");
            assert_eq!(
                decoded.ts,
                datetime!(2026-09-17 12:34:56 UTC),
                "{text}: the separator and the offset belong to the text, not to the \
                 instant it names"
            );
            assert_eq!(
                serde_json::to_string(&decoded).expect("a read-back envelope re-encodes"),
                concat!(
                    r#"{"seq":1,"ts":"2026-09-17T12:34:56Z","#,
                    r#""task_id":null,"kind":{"kind":"Resumed"}}"#,
                ),
                "reading a record must canonicalize it, or every re-write keeps the odd \
                 spelling its author used",
            );
        }
    }

    #[test]
    fn envelope_refuses_a_timestamp_that_is_not_rfc_3339_text() {
        for ts in [
            // What the `serde` derive on `time`'s own type writes with the
            // features docs/DESIGN.md fixes: lossless, and unreadable by
            // anything that is not `time`.
            r"[2026,260,12,34,56,0,0,0,0]",
            r#""2026-09-17T12:34:56""#,
            r#""2026-09-17T12:34:56+02""#,
            r#""1789212896""#,
            "1789212896",
            r#""""#,
        ] {
            let text =
                format!(r#"{{"seq":1,"ts":{ts},"task_id":null,"kind":{{"kind":"Resumed"}}}}"#);
            assert!(
                serde_json::from_str::<Event>(&text).is_err(),
                "{text} names no instant in the one format the ts column documents, so \
                 reading it must fail rather than invent a time",
            );
        }
    }

    #[test]
    fn envelope_refuses_an_instant_whose_year_rfc_3339_has_no_digits_for() {
        let event = envelope(
            1,
            datetime!(0000-01-01 00:30 +02:00),
            None,
            EventKind::Resumed,
        );
        assert!(
            serde_json::to_string(&event).is_err(),
            "in UTC that instant is a year before the calendar RFC 3339 writes, so the \
             only answer that is not a panic is a refusal"
        );
    }

    #[test]
    fn envelope_refuses_an_instant_with_no_utc_date_to_write() {
        let event = envelope(
            1,
            datetime!(9999-12-31 23:30 -01:00),
            None,
            EventKind::Resumed,
        );
        let text = serde_json::to_string(&event);
        assert!(
            text.is_err(),
            "one hour into UTC is a date past the last one there is; `time`'s unchecked \
             conversion panics here, and writing a record must fail instead of taking the \
             run down with it"
        );
    }

    #[test]
    fn queue_level_envelope_writes_its_absent_task_as_null() {
        let event = envelope(
            1,
            datetime!(2026-09-17 12:34:56 UTC),
            None,
            EventKind::PreflightStarted,
        );
        let text = serde_json::to_string(&event).expect("an envelope encodes as a string");
        assert_eq!(
            text,
            concat!(
                r#"{"seq":1,"ts":"2026-09-17T12:34:56Z","#,
                r#""task_id":null,"kind":{"kind":"PreflightStarted"}}"#,
            ),
            "a queue-level event is the events table's NULL task_id, and the key stays \
             present so every envelope the CLI prints has the same four keys",
        );

        let decoded: Event =
            serde_json::from_str(&text).expect("what the envelope writes is read back");
        assert_eq!(
            decoded, event,
            "no task stays no task through the round trip"
        );

        let spelled = concat!(
            r#"{"seq":1,"ts":"2026-09-17T12:34:56Z","#,
            r#""kind":{"kind":"PreflightStarted"}}"#,
        );
        let omitted: Event =
            serde_json::from_str(spelled).expect("a record with no task_id key at all is readable");
        assert_eq!(
            omitted, event,
            "an absent task_id and a null one are the same fact — the event is about the \
             queue — so the read side does not need the key the write side always spells"
        );
    }

    #[test]
    fn envelope_refuses_a_record_with_no_sequence_no_instant_or_no_kind() {
        for text in [
            r#"{"ts":"2026-09-17T12:34:56Z","task_id":null,"kind":{"kind":"Resumed"}}"#,
            r#"{"seq":1,"task_id":null,"kind":{"kind":"Resumed"}}"#,
            r#"{"seq":1,"ts":"2026-09-17T12:34:56Z","task_id":null}"#,
        ] {
            assert!(
                serde_json::from_str::<Event>(text).is_err(),
                "{text} leaves out a column that has no absent spelling, so a record that \
                 decoded anyway would be an event nobody recorded when, in what order, or \
                 what happened",
            );
        }
    }

    #[test]
    fn envelope_holds_the_payload_whose_kind_the_journal_indexes_on() {
        for kind in one_event_per_documented_entry() {
            let name = kind.discriminant();
            let event = envelope(1, datetime!(2026-09-17 12:34:56 UTC), None, kind.clone());
            let encoded = serde_json::to_value(&event).expect("an envelope encodes as JSON");
            let payload = encoded
                .get("kind")
                .expect("the envelope carries the payload beside its columns");
            assert_eq!(
                payload,
                &serde_json::to_value(&event.kind).expect("a catalog entry encodes as JSON"),
                "{name}: the payload inside the envelope and the payload the payload column \
                 holds must be one object, or a stored event differs from a streamed one",
            );
            assert_eq!(
                payload.get("kind").and_then(Value::as_str),
                Some(name),
                "{name}: the tag inside the envelope is the string the kind column stores",
            );

            let text = serde_json::to_string(&event).expect("an envelope encodes as a string");
            let decoded: Event =
                serde_json::from_str(&text).expect("what the envelope writes is read back");
            assert_eq!(decoded, event, "{name} survives the envelope round trip");
        }
    }
}

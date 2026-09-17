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
//! # Catalog entries that are not here yet
//!
//! `docs/DESIGN.md` lists 28 entries; 19 are defined below. The other 9 are
//! absent, and a test asserts their absence rather than trusting it:
//!
//! - Eight are deferred by the plan — `GateFinished` (`result: GateResult`),
//!   `AttemptFinished` (`usage: Option<Usage>`), `AttemptRecorded` (`record:
//!   AttemptRecord`), `ProviderDetected` (`capabilities: Capabilities`),
//!   `DecisionRaised` (`request: DecisionRequest`), `DecisionResolved`,
//!   `SelfHealingReport` and `TddExceptionUsed`. Each arrives with the task
//!   that emits it and gives it an `apply` arm, so nothing can journal an
//!   event whose effect on state no task has written yet.
//! - `GateStarted` (`kind: GateKind`) cannot be defined at all: `GateKind` is
//!   `gate.rs`, which no earlier task has written, and a payload naming it
//!   would not compile — which is the point of this catalog being a compile
//!   check. `Error::Gate` waits on the same type for the same reason
//!   (ADR-0001). It has a second problem waiting for T038: its documented
//!   field is also called `kind`, the key `#[serde(tag = "kind")]` already
//!   owns, so the derive refuses it until the field is renamed
//!   (ADR-0011 records the measurement).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::classify::FailureClass;
use crate::ids::AttemptId;
use crate::state::{PauseReason, Phase, Recovery, Stream};

/// Something that happened — to the queue, to a task, or to one run of a task.
///
/// Every field is a fact observed at the moment of the event, not a view of
/// current state: the journal is append-only and never rewritten, so what a
/// run looked like halfway through stays readable after it finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A human acknowledged a gate that had stopped the run.
    GateAcknowledged {
        /// Who acknowledged it.
        by: String,
        /// When, so an acknowledgement can be audited after the fact rather
        /// than taken on trust.
        at: OffsetDateTime,
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
            Self::GateAcknowledged { .. } => "GateAcknowledged",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EventKind;
    use crate::classify::FailureClass;
    use crate::ids::AttemptId;
    use crate::state::{PauseReason, Phase, Recovery, Stream};
    use serde_json::Value;
    use time::macros::datetime;

    /// A commit sha every entry below is built from, so a field that fails to
    /// encode is visible rather than mistaken for a placeholder.
    const SHA: &str = "0b78d3f1c2a4";

    /// The 19 entries `docs/DESIGN.md` documents whose payload types exist
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
        ("GateAcknowledged", &["by", "at"]),
    ];

    /// The nine entries this catalog does not define yet.
    ///
    /// Their absence is asserted, not assumed: an entry added ahead of its
    /// producer would start decoding, and the journal would begin accepting
    /// events the plan says nothing may emit until the task that writes that
    /// producer lands.
    ///
    /// The second half of each pair is the payload `docs/DESIGN.md` documents
    /// for the entry, minus the tag the test adds. The populated payload is
    /// what makes the refusal an assertion: `{"kind":"GateFinished"}` is
    /// refused for its missing field whether or not the entry exists, so a
    /// bare name would prove nothing either way. `GateStarted` is the one
    /// sample that renames a field, because its documented one is `kind` and
    /// the tag owns that key (ADR-0011).
    const DEFERRED_PAYLOADS: &[(&str, &str)] = &[
        ("GateStarted", r#""gate":"Verify""#),
        ("GateFinished", r#""result":"Passed""#),
        (
            "AttemptFinished",
            concat!(
                r#""attempt":2,"exit_code":0,"usage":null,"#,
                r#""session_id":null,"model_reported":null"#,
            ),
        ),
        ("AttemptRecorded", r#""record":{}"#),
        (
            "ProviderDetected",
            r#""provider":"codex","capabilities":{},"version":"0.1.0""#,
        ),
        (
            "TddExceptionUsed",
            r#""exception":"Documentation","reason":"docs only""#,
        ),
        (
            "DecisionRaised",
            concat!(
                r#""request":{"question":"which?","options":["a","b"],"#,
                r#""tradeoffs":"cost","impact":"queue","recommended":"a"}"#,
            ),
        ),
        (
            "DecisionResolved",
            r#""adr_path":"docs/adr/0011.md","answer":"a""#,
        ),
        (
            "SelfHealingReport",
            concat!(
                r#""attempt":2,"class":"AgentFailure","#,
                r#""repairs":["re-run fmt"],"outcome":"green""#,
            ),
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
            EventKind::GateAcknowledged {
                by: "operators.name".to_string(),
                at: datetime!(2026-09-17 12:34:56 UTC),
            },
        ]
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
}

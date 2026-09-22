//! `EventKind`: the catalog of everything a run can record in the journal.
//!
//! Every payload documented in `docs/DESIGN.md`'s "Event catalog" table is
//! listed here, `#[serde(tag = "kind")]` so a stored event's `kind` column
//! names its variant. Nothing may emit an event this enum does not contain.
//!
//! Seven variants are deliberately absent because their payload names a type
//! no earlier task has defined: `GateFinished`, `AttemptFinished`,
//! `ProviderDetected`, `TddExceptionUsed`, `DecisionRaised`,
//! `DecisionResolved` and `SelfHealingReport`. An eighth, `GateStarted`, is
//! absent for the same reason: its `kind: GateKind` field names a type
//! `gate.rs` has not yet introduced, matching the precedent set by
//! [`crate::Error::Gate`], whose `kind` field is a `String` today rather
//! than `GateKind`. Each is added by the task that defines its payload type,
//! which also adds its arm to the transition function.

use crate::{
    AttemptId, AttemptRecord, EventSeq, FailureClass, PauseReason, Phase, Recovery, Stream, TaskId,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A single record in the event journal: a position, a time, the task it
/// concerns and what happened.
///
/// `ts` serializes as RFC 3339 in UTC, matching the journal's on-disk and
/// wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// This event's monotonic, global position in the journal.
    pub seq: EventSeq,
    /// When this event was recorded.
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    /// The task this event concerns, if any.
    pub task_id: Option<TaskId>,
    /// What happened.
    pub kind: EventKind,
}

/// Everything a run can record in the event journal.
///
/// Serializes with an internal `kind` tag whose value is the variant name,
/// matching the `kind` column of the `events` table and [`EventKind::discriminant`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EventKind {
    /// A task was added to the queue.
    TaskQueued {
        /// The task's title.
        title: String,
    },
    /// Preflight checks for a task began.
    PreflightStarted,
    /// Preflight checks passed.
    PreflightPassed {
        /// The commit the task will be attempted from.
        base_sha: String,
    },
    /// Preflight checks failed.
    PreflightFailed {
        /// Why the preflight failed.
        class: FailureClass,
        /// A human-readable description of the failure.
        detail: String,
    },
    /// An attempt at a task began.
    AttemptStarted {
        /// The attempt's position within the task.
        attempt: AttemptId,
        /// The protocol the attempt runs under.
        protocol: String,
        /// The process id of the spawned agent.
        pid: u32,
        /// The commit the attempt started from.
        base_sha: String,
    },
    /// An attempt entered a new phase of its protocol.
    PhaseEntered {
        /// The attempt entering the phase.
        attempt: AttemptId,
        /// The phase entered.
        phase: Phase,
    },
    /// The agent produced a line of output.
    AgentOutput {
        /// The attempt the output came from.
        attempt: AttemptId,
        /// Which stream the output came from.
        stream: Stream,
        /// The output text.
        text: String,
    },
    /// The completion gates passed for an attempt.
    VerifyPassed {
        /// The attempt that passed.
        attempt: AttemptId,
    },
    /// The completion gates failed for an attempt.
    VerifyFailed {
        /// The attempt that failed.
        attempt: AttemptId,
        /// Why verification failed.
        class: FailureClass,
        /// A human-readable description of the failure.
        detail: String,
    },
    /// Publishing a verified attempt began.
    PublishStarted {
        /// The attempt being published.
        attempt: AttemptId,
        /// The commit being published.
        candidate_sha: String,
    },
    /// Publishing was verified against the remote.
    PublishVerified {
        /// The published commit.
        commit: String,
        /// The commit's sha as observed on the remote.
        remote_sha: String,
    },
    /// A task completed successfully.
    TaskDone {
        /// The task's published commit.
        commit: String,
    },
    /// A task failed and will not be retried automatically.
    TaskFailed {
        /// Why the task failed.
        class: FailureClass,
        /// A human-readable description of the failure.
        detail: String,
    },
    /// A task was cancelled.
    TaskCancelled {
        /// Why the task was cancelled.
        reason: String,
    },
    /// A task was paused.
    Paused {
        /// Why the task was paused.
        reason: PauseReason,
    },
    /// A paused task resumed.
    Resumed,
    /// A run was interrupted mid-phase, for example by a process kill.
    Interrupted {
        /// The phase the attempt was in when interrupted.
        phase: Phase,
    },
    /// The runner decided how to reconcile recorded state with reality after
    /// an interruption.
    RecoveryDecision {
        /// The recovery strategy chosen.
        decision: Recovery,
        /// A human-readable description of the decision.
        detail: String,
    },
    /// A human acknowledged a gate that required their attention.
    GateAcknowledged {
        /// Who acknowledged the gate.
        by: String,
        /// When the gate was acknowledged.
        at: OffsetDateTime,
    },
    /// An attempt's durable evidence was recorded: `VISION.md` §6, "every
    /// attempt is preserved separately." Boxed: an [`AttemptRecord`] carries
    /// a gate history and usage figures, and would otherwise make it the
    /// largest variant by a wide margin, bloating every [`EventKind`] value
    /// regardless of which variant it holds.
    AttemptRecorded {
        /// The evidence recorded.
        record: Box<AttemptRecord>,
    },
}

impl EventKind {
    /// Returns this event's variant name, matching the `kind` column of the
    /// `events` table and the `kind` tag used when serializing.
    #[must_use]
    pub fn discriminant(&self) -> &'static str {
        match self {
            EventKind::TaskQueued { .. } => "TaskQueued",
            EventKind::PreflightStarted => "PreflightStarted",
            EventKind::PreflightPassed { .. } => "PreflightPassed",
            EventKind::PreflightFailed { .. } => "PreflightFailed",
            EventKind::AttemptStarted { .. } => "AttemptStarted",
            EventKind::PhaseEntered { .. } => "PhaseEntered",
            EventKind::AgentOutput { .. } => "AgentOutput",
            EventKind::VerifyPassed { .. } => "VerifyPassed",
            EventKind::VerifyFailed { .. } => "VerifyFailed",
            EventKind::PublishStarted { .. } => "PublishStarted",
            EventKind::PublishVerified { .. } => "PublishVerified",
            EventKind::TaskDone { .. } => "TaskDone",
            EventKind::TaskFailed { .. } => "TaskFailed",
            EventKind::TaskCancelled { .. } => "TaskCancelled",
            EventKind::Paused { .. } => "Paused",
            EventKind::Resumed => "Resumed",
            EventKind::Interrupted { .. } => "Interrupted",
            EventKind::RecoveryDecision { .. } => "RecoveryDecision",
            EventKind::GateAcknowledged { .. } => "GateAcknowledged",
            EventKind::AttemptRecorded { .. } => "AttemptRecorded",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn ts_serializes_as_exact_rfc3339_utc() {
        let event = Event {
            seq: EventSeq::new(1),
            ts: datetime!(2024-01-15 10:30:00 UTC),
            task_id: Some(TaskId::new(3)),
            kind: EventKind::Resumed,
        };

        let value: serde_json::Value = serde_json::to_value(&event).expect("serialize to value");
        assert_eq!(value["ts"], "2024-01-15T10:30:00Z");
    }

    #[test]
    fn event_round_trips_through_json_preserving_the_instant() {
        let event = Event {
            seq: EventSeq::new(42),
            ts: datetime!(2024-01-15 10:30:00.5 UTC),
            task_id: None,
            kind: EventKind::TaskQueued {
                title: "Add widget".to_string(),
            },
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.seq, event.seq);
        assert_eq!(back.task_id, event.task_id);
        assert_eq!(back.kind, event.kind);
        assert_eq!(back.ts, event.ts);
        assert_eq!(
            back.ts.unix_timestamp_nanos(),
            event.ts.unix_timestamp_nanos()
        );
    }

    fn all_events() -> Vec<EventKind> {
        vec![
            EventKind::TaskQueued {
                title: "Add widget".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc123".to_string(),
            },
            EventKind::PreflightFailed {
                class: FailureClass::EnvironmentFailure,
                detail: "disk full".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "tdd".to_string(),
                pid: 4242,
                base_sha: "abc123".to_string(),
            },
            EventKind::PhaseEntered {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            },
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "running tests".to_string(),
            },
            EventKind::VerifyPassed {
                attempt: AttemptId::new(1),
            },
            EventKind::VerifyFailed {
                attempt: AttemptId::new(1),
                class: FailureClass::VerificationFailure,
                detail: "clippy failed".to_string(),
            },
            EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: "def456".to_string(),
            },
            EventKind::PublishVerified {
                commit: "def456".to_string(),
                remote_sha: "def456".to_string(),
            },
            EventKind::TaskDone {
                commit: "def456".to_string(),
            },
            EventKind::TaskFailed {
                class: FailureClass::AgentFailure,
                detail: "agent crashed".to_string(),
            },
            EventKind::TaskCancelled {
                reason: "superseded".to_string(),
            },
            EventKind::Paused {
                reason: PauseReason::Input,
            },
            EventKind::Resumed,
            EventKind::Interrupted {
                phase: Phase::Green,
            },
            EventKind::RecoveryDecision {
                decision: Recovery::Resume,
                detail: "journal complete through phase".to_string(),
            },
            EventKind::GateAcknowledged {
                by: "yalovoy".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
            EventKind::AttemptRecorded {
                record: Box::new(AttemptRecord {
                    id: AttemptId::new(1),
                    task: TaskId::new(1),
                    started: OffsetDateTime::UNIX_EPOCH,
                    ended: None,
                    model_configured: Some("claude-opus-4".to_string()),
                    model_reported: None,
                    session_id: None,
                    exit_reason: "completed".to_string(),
                    gates: Vec::new(),
                    usage: None,
                    base_sha: "abc123".to_string(),
                    candidate_sha: None,
                }),
            },
        ]
    }

    #[test]
    fn event_kind_has_exactly_twenty_variants() {
        let variants = all_events();
        assert_eq!(variants.len(), 20);

        // Exhaustive, wildcard-free match: a variant added to `EventKind`
        // without being listed here fails to compile instead of silently
        // under-counting.
        for event in variants {
            match event {
                EventKind::TaskQueued { .. }
                | EventKind::PreflightStarted
                | EventKind::PreflightPassed { .. }
                | EventKind::PreflightFailed { .. }
                | EventKind::AttemptStarted { .. }
                | EventKind::PhaseEntered { .. }
                | EventKind::AgentOutput { .. }
                | EventKind::VerifyPassed { .. }
                | EventKind::VerifyFailed { .. }
                | EventKind::PublishStarted { .. }
                | EventKind::PublishVerified { .. }
                | EventKind::TaskDone { .. }
                | EventKind::TaskFailed { .. }
                | EventKind::TaskCancelled { .. }
                | EventKind::Paused { .. }
                | EventKind::Resumed
                | EventKind::Interrupted { .. }
                | EventKind::RecoveryDecision { .. }
                | EventKind::GateAcknowledged { .. }
                | EventKind::AttemptRecorded { .. } => {}
            }
        }
    }

    #[test]
    fn every_event_round_trips_through_json() {
        for event in all_events() {
            let json = serde_json::to_string(&event).expect("serialize");
            let back: EventKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(event, back);
        }
    }

    /// The variant names of [`all_events`], in the same order, as written in
    /// `docs/DESIGN.md`'s event catalog table.
    fn all_event_names() -> Vec<&'static str> {
        vec![
            "TaskQueued",
            "PreflightStarted",
            "PreflightPassed",
            "PreflightFailed",
            "AttemptStarted",
            "PhaseEntered",
            "AgentOutput",
            "VerifyPassed",
            "VerifyFailed",
            "PublishStarted",
            "PublishVerified",
            "TaskDone",
            "TaskFailed",
            "TaskCancelled",
            "Paused",
            "Resumed",
            "Interrupted",
            "RecoveryDecision",
            "GateAcknowledged",
            "AttemptRecorded",
        ]
    }

    #[test]
    fn discriminant_matches_variant_name() {
        let events = all_events();
        let names = all_event_names();
        assert_eq!(events.len(), names.len());

        for (event, name) in events.into_iter().zip(names) {
            assert_eq!(event.discriminant(), name);
        }
    }

    #[test]
    fn tag_in_serialized_json_matches_discriminant() {
        for event in all_events() {
            let expected_kind = event.discriminant();
            let value: serde_json::Value =
                serde_json::to_value(&event).expect("serialize to value");
            assert_eq!(value["kind"], expected_kind);
        }
    }
}

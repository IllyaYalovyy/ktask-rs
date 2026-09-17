//! The vocabulary of a run: which phase it is in, why it stopped, and what
//! recovery decided.
//!
//! [`TaskState`] is the lifecycle those words describe; [`Phase`],
//! [`PauseReason`], [`Stream`] and [`Recovery`] are the words it is spoken with.
//!
//! These are names, not behaviour. Nothing here holds a lock, reads a clock or
//! touches a file, which is what lets the state machine `state::apply` will
//! hold, the event catalog that carries them, and every screen that displays a
//! run all speak the same words without any of them owning the others.
//!
//! The variants are exactly those `docs/DESIGN.md` fixes, spelled as it spells
//! them, because they are what the journal stores and what `--json` output
//! prints: a renamed or added variant silently changes durable data, so the
//! tests below pin the count and the encoding of each.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::classify::FailureClass;
use crate::ids::AttemptId;

/// Where a task stands in its lifecycle, and everything the last journal
/// record proved about it.
///
/// `Queued` → `Preflight` → `Running` → `Verifying` → `Publishing` →
/// `PublishedVerified` → `Done` is the path a task takes when nothing goes
/// wrong; [`TaskState::Remediating`] stands in for `Running` during the one
/// bounded second attempt a failed task is allowed, and
/// [`TaskState::Paused`] stands in wherever a run has to stop and wait. The
/// payloads are the reason this is one enum and not a name: a state that could
/// not say which attempt it belongs to, or which commit the remote was read
/// back holding, would leave the journal as the only place those facts lived,
/// and `state::apply` can stay pure — no I/O, no clock — only because the
/// state carries them.
///
/// This is durable data. The journal stores it in `serde`'s default
/// representation and `--json` output prints it, so the variants and their
/// field names are exactly the ones the `docs/DESIGN.md` Core types section
/// writes, and a later task cannot reshape one in passing without rewriting
/// every journal written before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskState {
    /// In the queue and not yet started: the state a parsed plan lands in.
    Queued,
    /// Running the checks that must hold before an agent is let near the tree.
    Preflight,
    /// An agent is working: this `attempt`, on this `phase`.
    Running {
        /// The attempt now running.
        attempt: AttemptId,
        /// The protocol step it is on.
        phase: Phase,
    },
    /// The second, bounded attempt at a task that failed once, carrying the
    /// failure bundle rather than a fresh start.
    Remediating {
        /// The remediation attempt, which is always later than the one that failed.
        attempt: AttemptId,
        /// The protocol step the remediation is on.
        phase: Phase,
    },
    /// The agent has finished; the gates are deciding whether that counts.
    Verifying {
        /// The attempt whose output is being verified.
        attempt: AttemptId,
    },
    /// The gates passed; the commit is being made and pushed.
    Publishing {
        /// The attempt being published.
        attempt: AttemptId,
    },
    /// The remote has been read back and holds `commit`. Work is not published
    /// until this state exists, because nothing is taken on an agent's say-so.
    PublishedVerified {
        /// The commit the remote was proved to hold.
        commit: String,
    },
    /// Finished, with its outcome recorded.
    Done,
    /// Finished, and a human has said so: what history records as closed
    /// rather than merely complete.
    Acknowledged {
        /// Who acknowledged it.
        by: String,
        /// When they did.
        at: OffsetDateTime,
    },
    /// Standing still for a reason outside the work, and holding where to go
    /// back to. The boxed state is what makes a pause durable: a supervisor
    /// that dies mid-pause resumes that same wait rather than guessing one.
    Paused {
        /// Why the run stopped.
        reason: PauseReason,
        /// The state to return to. Inside the pause, because a pause without
        /// one is not resumable, only abandoned.
        resume_to: Box<TaskState>,
    },
    /// Finished, and not done, classified by the response it needs.
    Failed {
        /// What kind of failure this is, which is what the next move is chosen
        /// from.
        class: FailureClass,
        /// The sentence that goes beside the class, never in place of it.
        detail: String,
    },
    /// Dropped by a human decision, so the queue may proceed past it.
    Cancelled,
}

impl TaskState {
    /// Whether this state offers the supervisor anywhere further to go.
    ///
    /// Unlike [`TaskState::Paused`], none of the four states this returns
    /// `true` for holds a state to return to: the queue has already decided
    /// what to do about them — proceed past a [`TaskState::Cancelled`] task,
    /// stop the run at a [`TaskState::Failed`] one, which is the "terminal
    /// failure" `docs/CONTRACT.md` says a `run` stops at. Putting one back in
    /// motion is a human command that starts *new* work: `retry` begins a
    /// fresh remediation attempt rather than continuing the failed one, so the
    /// terminal state is the record an attempt was seeded from, not a place a
    /// transition leaves. A [`TaskState::Paused`] task is the contrast case,
    /// and stays non-terminal even above a terminal `resume_to`: it is waiting,
    /// not finished, and the type says so.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done | Self::Acknowledged { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }

    /// Whether the task is standing still with somewhere to come back to.
    ///
    /// Exactly [`TaskState::Paused`], regardless of what it boxes: a pause
    /// above a finished state is still a pause, which is why the answer is not
    /// read out of the boxed state.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. })
    }

    /// The variant's own name, spelled as `docs/DESIGN.md` spells it.
    ///
    /// This is the identity of the state, not a rendering of it: it is what
    /// `state::apply` hands to `Error::InvalidTransition { from }` to say which
    /// state refused an event, and what a screen prints beside the payload. The
    /// lowercase, hyphenated forms `--json` output uses belong to whoever is
    /// printing, so they are not here.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Preflight => "Preflight",
            Self::Running { .. } => "Running",
            Self::Remediating { .. } => "Remediating",
            Self::Verifying { .. } => "Verifying",
            Self::Publishing { .. } => "Publishing",
            Self::PublishedVerified { .. } => "PublishedVerified",
            Self::Done => "Done",
            Self::Acknowledged { .. } => "Acknowledged",
            Self::Paused { .. } => "Paused",
            Self::Failed { .. } => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// The step a work protocol is on, as the queue and inspector display it.
///
/// One enum serves all three v1 protocols, so it carries steps that any single
/// protocol never reaches: `Implement` belongs to `direct`, the
/// red/green/refactor trio to `tdd`, and `Goal`, `Scope`, `AcceptanceTests`,
/// `Review`, `Harden` and `DoneCheck` to `spec-first`, which is why no
/// `SpecFirst` variant exists. `Verify` and `Publish` belong to every protocol:
/// no protocol may end without them.
///
/// Declared in no ordering that means anything — the protocol decides the
/// sequence — so this derives no [`Ord`]: comparing two phases for less-than
/// would claim a fact only the protocol in force can supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// `spec-first`: what the task is for, written down before anything is.
    Goal,
    /// `spec-first`: which files the work may reach, and which it may not.
    Scope,
    /// `spec-first`: the tests that must pass before the phase counts as done.
    AcceptanceTests,
    /// `direct`: the one implementation phase that protocol allows.
    Implement,
    /// `tdd`: tests only; production paths stay read-only, and the new test
    /// must fail.
    Red,
    /// `tdd`: implementation changes are permitted, and the new test must pass.
    Green,
    /// `tdd`: cleanup, while the targeted tests stay green.
    Refactor,
    /// `spec-first`: the work read critically, by a reviewer if one is configured.
    Review,
    /// `spec-first`: robustness work the acceptance tests do not demand.
    Harden,
    /// `spec-first`: the completion checks, run before the gates rather than as one.
    DoneCheck,
    /// The mandatory verification gates. Every protocol ends here.
    Verify,
    /// Commit, push, and prove the remote holds the commit.
    Publish,
}

/// Why a run is standing still, and what it is waiting for.
///
/// The `until` of [`PauseReason::Limit`] is the only field any of this
/// vocabulary carries, which is why this is the one enum here that is `Clone`
/// rather than [`Copy`]. The instant is what makes a limit wait resumable after
/// a restart: the journal holds it, so a supervisor that died mid-wake resumes
/// to the same wait rather than a shorter one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// A provider refused work. `until` is the reset time when one was
    /// advertised, and [`None`] when it was not.
    Limit {
        /// The instant to resume at, if the provider said when.
        until: Option<OffsetDateTime>,
    },
    /// Stopped on a question only a human can answer.
    Input,
    /// Stopped at a gate that waits for a human decision.
    HumanGate,
    /// Stopped by a signal or a crash; recovery decides where to resume.
    Interrupted,
    /// Stopped by something outside the supervisor that it cannot fix: a dirty
    /// tree, a missing tool, no remote.
    Blocked,
}

/// Which of an agent's two output streams a line arrived on.
///
/// Kept from the moment a line is read, because a screen that shows only the
/// agent's own words and a screen that shows its complaints are different
/// questions, and the distinction is unrecoverable after the two are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    /// What the agent printed to be read as its work.
    Stdout,
    /// What the agent printed to be read as a problem.
    Stderr,
}

/// What recovery concluded about an interrupted run.
///
/// The three answers are the only ones a journal replay can give: the work was
/// not started, was started and unappliable, or was applied and already
/// recorded. Anything else is a question, not a recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovery {
    /// The journal ends mid-phase and nothing was published: resume the phase.
    Resume,
    /// The run cannot be resumed safely, so record it as interrupted and stop.
    MarkInterrupted,
    /// The transition being replayed is already in the journal: apply nothing.
    AlreadyApplied,
}

#[cfg(test)]
mod tests {
    use super::{PauseReason, Phase, Recovery, Stream, TaskState};
    use crate::{AttemptId, FailureClass};
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;

    /// Every `Phase`, in the order `docs/DESIGN.md` declares them.
    const PHASES: [Phase; 12] = [
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
    ];

    /// The same twelve names as `docs/DESIGN.md` spells them, written out a
    /// second time so a rename cannot slip through by matching itself.
    const PHASE_NAMES: [&str; 12] = [
        "Goal",
        "Scope",
        "AcceptanceTests",
        "Implement",
        "Red",
        "Green",
        "Refactor",
        "Review",
        "Harden",
        "DoneCheck",
        "Verify",
        "Publish",
    ];

    /// Every `PauseReason`. `Limit` is listed with no reset time because the
    /// instant it may carry is not part of the variant's identity.
    const PAUSE_REASONS: [PauseReason; 5] = [
        PauseReason::Limit { until: None },
        PauseReason::Input,
        PauseReason::HumanGate,
        PauseReason::Interrupted,
        PauseReason::Blocked,
    ];

    /// The four `PauseReason`s that carry nothing, by their documented names.
    const PAUSE_REASON_NAMES: [&str; 4] = ["Input", "HumanGate", "Interrupted", "Blocked"];

    /// Encodes `value`, insists it carries the documented name, reads it back.
    fn round_trips<T>(value: &T, name: &str)
    where
        T: Clone + serde::Serialize + DeserializeOwned + PartialEq + Debug,
    {
        let encoded = serde_json::to_string(value).expect("vocabulary encodes as JSON");
        assert_eq!(
            encoded,
            format!("\"{name}\""),
            "{name} must encode as its own name"
        );
        let decoded: T =
            serde_json::from_str(&encoded).expect("a documented encoding is read back");
        assert_eq!(&decoded, value, "{name} must survive the round trip");
    }

    #[test]
    fn phase_has_the_twelve_variants_docs_design_md_names() {
        assert_eq!(PHASES.len(), 12);
        assert_eq!(PHASE_NAMES.len(), PHASES.len());
        for (phase, name) in PHASES.iter().zip(PHASE_NAMES) {
            round_trips(phase, name);
        }
    }

    #[test]
    fn pause_reason_has_the_five_variants_docs_design_md_names() {
        assert_eq!(PAUSE_REASONS.len(), 5);
        assert_eq!(PAUSE_REASON_NAMES.len(), PAUSE_REASONS.len() - 1);
        for (reason, name) in PAUSE_REASONS[1..].iter().zip(PAUSE_REASON_NAMES) {
            round_trips(reason, name);
        }

        let encoded = serde_json::to_string(&PauseReason::Limit { until: None })
            .expect("a limit pause encodes as JSON");
        assert_eq!(encoded, r#"{"Limit":{"until":null}}"#);
        assert_eq!(
            serde_json::from_str::<PauseReason>(&encoded).expect("the encoding is read back"),
            PauseReason::Limit { until: None },
        );
    }

    #[test]
    fn a_paused_run_keeps_the_instant_it_waits_until_through_json() {
        let until = time::macros::datetime!(2026-09-17 12:34:56 UTC);
        let paused = PauseReason::Limit { until: Some(until) };
        let encoded = serde_json::to_string(&paused).expect("a reset time encodes as JSON");
        match serde_json::from_str::<PauseReason>(&encoded).expect("the encoding is read back") {
            PauseReason::Limit { until: decoded } => assert_eq!(decoded, Some(until)),
            other => panic!("a limit pause must come back as a limit pause, got {other:?}"),
        }
    }

    #[test]
    fn stream_has_the_two_variants_docs_design_md_names() {
        const STREAMS: [Stream; 2] = [Stream::Stdout, Stream::Stderr];
        const NAMES: [&str; 2] = ["Stdout", "Stderr"];
        assert_eq!(STREAMS.len(), 2);
        for (stream, name) in STREAMS.iter().zip(NAMES) {
            round_trips(stream, name);
        }
    }

    #[test]
    fn recovery_has_the_three_variants_docs_design_md_names() {
        const RECOVERIES: [Recovery; 3] = [
            Recovery::Resume,
            Recovery::MarkInterrupted,
            Recovery::AlreadyApplied,
        ];
        const NAMES: [&str; 3] = ["Resume", "MarkInterrupted", "AlreadyApplied"];
        assert_eq!(RECOVERIES.len(), 3);
        for (recovery, name) in RECOVERIES.iter().zip(NAMES) {
            round_trips(recovery, name);
        }
    }

    #[test]
    fn the_vocabulary_refuses_a_name_no_document_names() {
        for rejected in ["SpecFirst", "implement", "Done", ""] {
            assert!(
                serde_json::from_str::<Phase>(&format!("\"{rejected}\"")).is_err(),
                "{rejected} is not a phase and must not deserialise as one",
            );
        }
        assert!(serde_json::from_str::<PauseReason>("\"Limit\"").is_err());
        assert!(serde_json::from_str::<Stream>("\"Output\"").is_err());
        assert!(serde_json::from_str::<Recovery>("\"Restart\"").is_err());
    }

    /// One instance of every `TaskState`, in the order `docs/DESIGN.md`
    /// declares them. Written out by hand rather than generated, because the
    /// point of the list is that a person named each entry.
    fn one_state_per_variant() -> [TaskState; 12] {
        [
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            TaskState::PublishedVerified {
                commit: "b7d1f3a".to_owned(),
            },
            TaskState::Done,
            TaskState::Acknowledged {
                by: "illya".to_owned(),
                at: time::macros::datetime!(2026-09-17 12:34:56 UTC),
            },
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Green,
                }),
            },
            TaskState::Failed {
                class: FailureClass::ProviderLimit,
                detail: "429 from the provider".to_owned(),
            },
            TaskState::Cancelled,
        ]
    }

    /// The same twelve names as `docs/DESIGN.md` spells them, written out a
    /// second time so a rename cannot slip through by matching itself.
    const TASK_STATE_NAMES: [&str; 12] = [
        "Queued",
        "Preflight",
        "Running",
        "Remediating",
        "Verifying",
        "Publishing",
        "PublishedVerified",
        "Done",
        "Acknowledged",
        "Paused",
        "Failed",
        "Cancelled",
    ];

    /// Which of the twelve a supervisor does no further work on, in the same
    /// order as [`one_state_per_variant`].
    const TERMINAL: [bool; 12] = [
        false, false, false, false, false, false, false, true, true, false, true, true,
    ];

    /// Which of the twelve is standing still rather than finished, likewise.
    const PAUSED: [bool; 12] = [
        false, false, false, false, false, false, false, false, false, true, false, false,
    ];

    /// Encodes `state` and reads the encoding straight back: the journal stores
    /// exactly this text, so a state that does not survive this is a state that
    /// cannot be recovered from a crash.
    fn state_through_json(state: &TaskState) -> TaskState {
        let encoded = serde_json::to_string(state).expect("a task state encodes as JSON");
        serde_json::from_str(&encoded).expect("a documented state encoding is read back")
    }

    #[test]
    fn task_state_has_the_twelve_variants_docs_design_md_names() {
        let states = one_state_per_variant();
        assert_eq!(states.len(), 12);
        assert_eq!(TASK_STATE_NAMES.len(), states.len());
        for (state, name) in states.iter().zip(TASK_STATE_NAMES) {
            assert_eq!(
                state.name(),
                name,
                "name() must report the documented variant"
            );
            let encoded = serde_json::to_string(state).expect("a task state encodes as JSON");
            assert!(
                encoded == format!("\"{name}\"") || encoded.starts_with(&format!("{{\"{name}\":")),
                "{name} must be tagged with the name the document gives it, got {encoded}"
            );
            assert_eq!(
                state_through_json(state),
                *state,
                "{name} must survive the JSON round trip"
            );
        }
    }

    #[test]
    fn a_state_payload_encodes_the_fields_docs_design_md_lists() {
        let running = TaskState::Running {
            attempt: AttemptId::new(2),
            phase: Phase::Implement,
        };
        assert_eq!(
            serde_json::to_string(&running).expect("a running state encodes as JSON"),
            r#"{"Running":{"attempt":2,"phase":"Implement"}}"#
        );
        let remediating = TaskState::Remediating {
            attempt: AttemptId::new(3),
            phase: Phase::Refactor,
        };
        assert_eq!(
            serde_json::to_string(&remediating).expect("a remediation encodes as JSON"),
            r#"{"Remediating":{"attempt":3,"phase":"Refactor"}}"#
        );
        let published = TaskState::PublishedVerified {
            commit: "b7d1f3a".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&published).expect("a published commit encodes as JSON"),
            r#"{"PublishedVerified":{"commit":"b7d1f3a"}}"#
        );
        let failed = TaskState::Failed {
            class: FailureClass::ProviderLimit,
            detail: "429 from the provider".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&failed).expect("a failure encodes as JSON"),
            r#"{"Failed":{"class":"ProviderLimit","detail":"429 from the provider"}}"#
        );
        let at = time::macros::datetime!(2026-09-17 12:34:56.123456789 UTC);
        let acknowledged = TaskState::Acknowledged {
            by: "illya".to_owned(),
            at,
        };
        let encoded =
            serde_json::to_string(&acknowledged).expect("an acknowledgement encodes as JSON");
        assert!(
            encoded.starts_with(r#"{"Acknowledged":{"#),
            "an acknowledgement is tagged with its variant, got {encoded}"
        );
        assert!(
            encoded.contains(r#""by":"illya""#),
            "who acknowledged is kept beside the instant, got {encoded}"
        );
        assert!(
            encoded.contains(r#""at":"#),
            "when it was acknowledged is kept, got {encoded}"
        );
        let decoded = state_through_json(&acknowledged);
        let TaskState::Acknowledged { at: kept, .. } = decoded else {
            panic!("an acknowledgement must come back as one, got {decoded:?}");
        };
        assert_eq!(
            kept, at,
            "an acknowledgement instant must survive to the nanosecond"
        );

        let stamped: serde_json::Value =
            serde_json::from_str(&encoded).expect("an acknowledgement is JSON");
        let vocabulary: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&PauseReason::Limit { until: Some(at) })
                .expect("a pause reason encodes as JSON"),
        )
        .expect("a pause reason is JSON");
        assert_eq!(
            stamped
                .get("Acknowledged")
                .and_then(|field| field.get("at")),
            vocabulary.get("Limit").and_then(|field| field.get("until")),
            "a state must timestamp an instant the way this vocabulary already does, \
             not invent a form of its own"
        );
    }

    #[test]
    fn a_pause_carries_the_state_it_resumes_to_inside_itself() {
        let paused = TaskState::Paused {
            reason: PauseReason::Input,
            resume_to: Box::new(TaskState::Queued),
        };
        assert_eq!(
            serde_json::to_string(&paused).expect("a paused state encodes as JSON"),
            r#"{"Paused":{"reason":"Input","resume_to":"Queued"}}"#,
            "the state to resume into belongs inside the pause, not beside it"
        );
    }

    #[test]
    fn a_pause_nested_in_a_pause_keeps_its_own_resume_state_and_instant() {
        let until = time::macros::datetime!(2026-09-17 12:34:56 UTC);
        let nested = TaskState::Paused {
            reason: PauseReason::Limit { until: Some(until) },
            resume_to: Box::new(TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(2),
                    phase: Phase::Red,
                }),
            }),
        };
        let decoded = state_through_json(&nested);
        assert_eq!(decoded, nested, "two levels of pause must both come back");
        let TaskState::Paused { reason, resume_to } = &decoded else {
            panic!("a paused state must return as a pause, got {decoded:?}");
        };
        assert_eq!(reason, &PauseReason::Limit { until: Some(until) });
        let TaskState::Paused {
            reason: inner_reason,
            resume_to: inner,
        } = resume_to.as_ref()
        else {
            panic!("the inner pause must return as a pause, got {resume_to:?}");
        };
        assert_eq!(
            inner_reason,
            &PauseReason::Interrupted,
            "the inner pause must keep its own reason, not the outer one"
        );
        assert_eq!(
            **inner,
            TaskState::Running {
                attempt: AttemptId::new(2),
                phase: Phase::Red
            },
            "the phase a nested pause resumes into must be the one that was stored"
        );
    }

    #[test]
    fn terminal_states_are_the_four_a_supervisor_does_no_further_work_on() {
        let states = one_state_per_variant();
        for ((state, name), terminal) in states.iter().zip(TASK_STATE_NAMES).zip(TERMINAL) {
            assert_eq!(
                state.is_terminal(),
                terminal,
                "{name} disagrees about being terminal"
            );
        }
    }

    #[test]
    fn only_a_paused_state_reports_itself_paused() {
        let states = one_state_per_variant();
        for ((state, name), paused) in states.iter().zip(TASK_STATE_NAMES).zip(PAUSED) {
            assert_eq!(
                state.is_paused(),
                paused,
                "{name} disagrees about being paused"
            );
        }
    }

    #[test]
    fn a_pause_is_neither_terminal_nor_finished_by_what_it_would_resume_into() {
        let over_done = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Done),
        };
        assert!(
            over_done.is_paused(),
            "a pause still stands still, whatever it would resume into"
        );
        assert!(
            !over_done.is_terminal(),
            "a pause still owes a resume, even above a finished state"
        );
        let TaskState::Paused { resume_to, .. } = &over_done else {
            panic!("the state built above is a pause");
        };
        assert!(
            resume_to.is_terminal(),
            "the boxed state is reported on its own merits"
        );
        assert!(!resume_to.is_paused());
    }

    #[test]
    fn a_state_encoding_refuses_what_docs_design_md_does_not_define() {
        for rejected in [
            "\"Waiting\"",
            "\"running\"",
            r#"{"Running":{"attempt":2}}"#,
            r#"{"Running":{"attempt":2,"phase":"SpecFirst"}}"#,
            r#"{"Paused":{"reason":"Input"}}"#,
            r#"{"Failed":{"class":"ProviderLimit"}}"#,
        ] {
            assert!(
                serde_json::from_str::<TaskState>(rejected).is_err(),
                "{rejected} is not a documented state encoding and must not be read as one"
            );
        }
    }
}

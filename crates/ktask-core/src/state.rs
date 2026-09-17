//! The vocabulary of a run: which phase it is in, why it stopped, and what
//! recovery decided.
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
    use super::{PauseReason, Phase, Recovery, Stream};
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
}

//! Work protocols: typed, ordered sequences of [`Phase`]s a task's attempt
//! executes, per VISION.md §9.
//!
//! "How you work is configurable; what done means is not... every protocol
//! must terminate in the mandatory verify-publish gates." That constraint
//! is checked structurally here, at construction, rather than left to the
//! convention of whoever writes the next protocol: this module's private
//! `checked` constructor panics if the phase list handed to it does not end
//! in [`Phase::Verify`] then [`Phase::Publish`], and both
//! [`Protocol::direct`] and [`Protocol::tdd`] are built through it.

use crate::{GateKind, Phase};

/// Which paths a [`PhaseSpec`]'s agent may modify while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WriteScope {
    /// The agent may modify any path.
    All,
    /// The agent may modify test paths only; production code is read-only.
    ///
    /// Used by the `tdd` protocol's `Red` phase (VISION.md §9): the runner
    /// enforces this via configured test-path globs, not the agent's word.
    TestsOnly,
    /// The agent may not modify any path.
    None,
}

/// One phase of a [`Protocol`]: what it is, what it may write, the
/// mechanical gate that must pass before it counts as done, and whether it
/// records evidence with the attempt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhaseSpec {
    /// Which phase this is.
    pub phase: Phase,
    /// What the agent may write while this phase is active.
    pub write_scope: WriteScope,
    /// The mechanical gate that must pass for this phase to complete, if
    /// any. `None` for phases that are not gated by a runner command (for
    /// example a git action).
    pub gate: Option<GateKind>,
    /// Whether this phase's outcome is recorded as attempt evidence.
    pub records_evidence: bool,
}

/// A named, ordered sequence of [`PhaseSpec`]s a task's attempt executes.
///
/// Only [`Protocol::direct`] and [`Protocol::tdd`] exist today; both are
/// built through a private constructor that panics if the phase list does
/// not end in [`Phase::Verify`] then [`Phase::Publish`] — the one part of
/// "how you work" that VISION.md §9 does not leave configurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol {
    /// The protocol's name, as used in task/attempt records.
    pub name: &'static str,
    /// The ordered phases this protocol runs.
    pub phases: Vec<PhaseSpec>,
}

/// Builds a [`Protocol`], panicking if `phases` does not end in
/// [`Phase::Verify`] then [`Phase::Publish`].
///
/// This is the only way this module constructs a [`Protocol`], so no
/// built-in protocol can skip the mandatory completion gates by accident —
/// the check runs once, at the call site inside [`Protocol::direct`] or
/// [`Protocol::tdd`], not against every attempt.
fn checked(name: &'static str, phases: Vec<PhaseSpec>) -> Protocol {
    let mut tail = phases.iter().rev();
    let ends_correctly = tail.next().is_some_and(|spec| spec.phase == Phase::Publish)
        && tail.next().is_some_and(|spec| spec.phase == Phase::Verify);
    assert!(
        ends_correctly,
        "protocol {name:?} must end with Verify then Publish"
    );
    Protocol { name, phases }
}

impl Protocol {
    /// v0.1's single-phase protocol: implement, then the mandatory
    /// completion gates.
    #[must_use]
    pub fn direct() -> Protocol {
        checked(
            "direct",
            vec![
                PhaseSpec {
                    phase: Phase::Implement,
                    write_scope: WriteScope::All,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        )
    }

    /// v0.1's runner-enforced red/green/refactor protocol (VISION.md §9).
    #[must_use]
    pub fn tdd() -> Protocol {
        checked(
            "tdd",
            vec![
                PhaseSpec {
                    phase: Phase::Red,
                    write_scope: WriteScope::TestsOnly,
                    gate: Some(GateKind::Targeted),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Green,
                    write_scope: WriteScope::All,
                    gate: Some(GateKind::Targeted),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Refactor,
                    write_scope: WriteScope::All,
                    gate: Some(GateKind::Targeted),
                    records_evidence: false,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_two_phases(protocol: &Protocol) -> Vec<Phase> {
        protocol.phases[protocol.phases.len() - 2..]
            .iter()
            .map(|spec| spec.phase)
            .collect()
    }

    #[test]
    fn direct_ends_with_verify_then_publish() {
        assert_eq!(
            last_two_phases(&Protocol::direct()),
            vec![Phase::Verify, Phase::Publish]
        );
    }

    #[test]
    fn tdd_ends_with_verify_then_publish() {
        assert_eq!(
            last_two_phases(&Protocol::tdd()),
            vec![Phase::Verify, Phase::Publish]
        );
    }

    #[test]
    fn tdd_gates_red_and_green_on_the_targeted_command_before_verify() {
        let phases = Protocol::tdd().phases;
        let red = phases.iter().find(|s| s.phase == Phase::Red).unwrap();
        let green = phases.iter().find(|s| s.phase == Phase::Green).unwrap();
        assert_eq!(red.write_scope, WriteScope::TestsOnly);
        assert_eq!(red.gate, Some(GateKind::Targeted));
        assert_eq!(green.write_scope, WriteScope::All);
        assert_eq!(green.gate, Some(GateKind::Targeted));
    }

    #[test]
    #[should_panic(expected = "must end with Verify then Publish")]
    fn a_constructor_that_omits_verify_then_publish_panics() {
        checked(
            "broken",
            vec![PhaseSpec {
                phase: Phase::Implement,
                write_scope: WriteScope::All,
                gate: None,
                records_evidence: true,
            }],
        );
    }

    #[test]
    #[should_panic(expected = "must end with Verify then Publish")]
    fn a_constructor_that_puts_publish_before_verify_panics() {
        checked(
            "broken",
            vec![
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
            ],
        );
    }

    #[test]
    fn direct_and_tdd_have_distinct_names() {
        assert_eq!(Protocol::direct().name, "direct");
        assert_eq!(Protocol::tdd().name, "tdd");
    }
}

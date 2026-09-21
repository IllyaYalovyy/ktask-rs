//! What it means to work on a task: a protocol, as a typed sequence of phases.
//!
//! VISION.md §9 splits a run into two state machines. [`crate::TaskState`] is
//! the outer one — custody of a task, and fixed for every task. What happens
//! *inside* `running` is the inner one: a **work protocol**, an opinionated
//! definition of the work, made of [`PhaseSpec`]s that each declare which paths
//! the agent may edit, which mechanical check decides the phase, and what
//! evidence the phase must leave behind. The current protocol and phase are
//! first-class state — [`crate::EventKind::PhaseEntered`] journals them and the
//! queue and inspector display them.
//!
//! This module is that declaration and nothing else: no I/O, no clock, no
//! `git`. It answers "what does this phase permit, and what proves it"; reading
//! a diff, running a gate and storing evidence are the runner's, because a
//! protocol that could act would be a protocol an agent could talk to.
//!
//! # The two protocols v1 ships
//!
//! [`direct`] is §9's *single implementation phase, then the mandatory
//! completion gates*: one [`Phase::Implement`] over the whole tree, then
//! [`Phase::Verify`], then [`Phase::Publish`]. [`tdd`] is §9's
//! red/green/refactor: [`Phase::Red`], where the agent is held to the project's
//! test paths and production paths stay read-only; [`Phase::Green`], where the
//! implementation opens up and that same new test has to pass; and
//! [`Phase::Refactor`], cleanup while those targeted tests stay green. Both
//! protocols end the same way.
//!
//! §9's third protocol, `spec-first`, is marked v0.2 there and has no
//! constructor here. [`Phase`] already carries its steps — `docs/DESIGN.md`
//! says so, which is why no later task has to widen that enum and why it has no
//! `SpecFirst` variant.
//!
//! # Where the ending comes from
//!
//! §9's constitution is structural, not advisory: "every protocol must
//! terminate in the mandatory verify-publish gates, every loop must be bounded,
//! gates cannot be removed." No constructor writes its own ending. Each hands
//! its body to `assemble`, which appends the [`Phase::Verify`] and
//! [`Phase::Publish`] pair to whatever body it is handed, so the two
//! constructors cannot agree to skip verification any more than they can agree
//! to skip a gate. [`Protocol::phases`] is a public field because the runner
//! and the TUI read it, so what closes the door on a hand-built protocol that
//! skips the ending is the test ledger in the tests below: every protocol v1
//! can build is in it, and each is checked against its ending there.
//!
//! # What a phase's evidence is
//!
//! A phase records evidence when the claim it makes is not already a gate
//! result. VISION.md §9 names two such phases: "RED and GREEN evidence
//! (command, output, tree hash) is stored with the attempt" — the claim *the
//! test failed before the implementation existed* is unrecoverable after the
//! fact unless it was captured then, which is exactly why no final test run can
//! prove tests were written first. A phase whose check *is* a gate — a targeted
//! run, the full suite — needs no second copy of what that gate journals with
//! the attempt as [`crate::GateResult`]. [`Phase::Publish`] is the odd one out
//! at the other end: no [`GateKind`] spells publication, so the proof §3's
//! invariant 7 demands — the commit a fetch brought back — is evidence the
//! phase records itself.

use crate::gate::GateKind;
use crate::state::Phase;

/// Which paths of the task worktree an agent may modify while a phase is in
/// force.
///
/// The scope is what makes VISION.md §9's red phase real: "the agent may add or
/// modify tests only; production paths are read-only" is a rule about files,
/// and a rule about files nobody checks is a sentence in a prompt. What "a test
/// path" means is the project's `test_globs` (`docs/DESIGN.md`), which is
/// language configuration rather than protocol configuration: the scope names
/// the *kind* of access a phase grants, and the caller resolves it to paths
/// with the globs the language profile supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteScope {
    /// Every path in the worktree: an implementation phase.
    All,
    /// Only the paths the project's test globs name. Everything else stays
    /// read-only — production code, and with it the gate configuration the run
    /// is judged by, which ADR-0067 refuses on every attempt regardless of
    /// scope.
    TestsOnly,
    /// No path. The phase runs commands and reads results; it edits nothing.
    /// Verification and publication are the two phases that declare this, and
    /// they declare it because VISION.md §10 classes a dirty tree at
    /// verification time a `policy_failure`.
    None,
}

/// One phase of a protocol, as the protocol declares it.
///
/// A declaration, not a record: it says what a phase owes, not what happened in
/// it. What happened belongs to the journal and the evidence tree instead:
/// [`crate::EventKind::PhaseEntered`] marks the step, and the
/// [`crate::AttemptRecord`] a run leaves behind holds what each check reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseSpec {
    /// Which step of the lifecycle this is, as the queue and the inspector name
    /// it.
    pub phase: Phase,
    /// Which paths the agent may modify here.
    pub write_scope: WriteScope,
    /// The mechanical check that decides the phase, or [`None`] when no gate
    /// decides it and only the evidence the phase records does.
    pub gate: Option<GateKind>,
    /// Whether the phase's own evidence is stored with the attempt, beyond
    /// whatever its gate journals. See the module's *What a phase's evidence
    /// is*.
    pub records_evidence: bool,
}

/// A work protocol: a name, and the phases worked in order.
///
/// Protocols are chosen per task and are not user-definable in v1 (VISION.md
/// §9), which is why this is a struct built by [`direct`] and [`tdd`] rather
/// than something parsed from a file: those two are the whole set, and a
/// workflow an agent could assemble is a workflow an agent could un-verify.
/// The name is the word a project's `default_protocol` holds and the word
/// [`crate::EventKind::AttemptStarted`] journals, so it is a `&'static str`
/// rather than a `String` a caller is free to misspell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol {
    /// Which protocol this is: `direct` or `tdd`.
    pub name: &'static str,
    /// The phases, in the order they are worked. Both v1 protocols end
    /// [`Phase::Verify`] then [`Phase::Publish`] — see [`direct`] and [`tdd`]
    /// for the two sequences and *Where the ending comes from* above for where
    /// that ending is appended.
    pub phases: Vec<PhaseSpec>,
}

/// Give a protocol body the ending every protocol has, and name the result.
///
/// The one door a body passes through on its way to becoming a protocol: the
/// two mandatory completion phases are appended here rather than written by
/// each constructor, because "the two of them decided not to verify" is the
/// failure mode VISION.md §9 exists to make impossible.
fn assemble(name: &'static str, mut phases: Vec<PhaseSpec>) -> Protocol {
    phases.extend([
        PhaseSpec {
            phase: Phase::Verify,
            write_scope: WriteScope::None,
            gate: Some(GateKind::Verify),
            records_evidence: false,
        },
        PhaseSpec {
            phase: Phase::Publish,
            write_scope: WriteScope::None,
            gate: None,
            records_evidence: true,
        },
    ]);
    Protocol { name, phases }
}

/// The `direct` protocol: one implementation phase, then the completion gates.
///
/// VISION.md §9's v0.1 default and the behaviour a task gets when its project
/// writes no `default_protocol`. The implementation phase runs the targeted
/// check while the agent works — the fast edit-loop gate §8 lists — because a
/// phase with no check at all is a phase that ended on the agent's word, which
/// §3's invariant 4 forbids. See `assemble` for where the ending comes from.
#[must_use]
pub fn direct() -> Protocol {
    assemble(
        "direct",
        vec![PhaseSpec {
            phase: Phase::Implement,
            write_scope: WriteScope::All,
            gate: Some(GateKind::Targeted),
            records_evidence: false,
        }],
    )
}

/// The `tdd` protocol: red, green, refactor, then the completion gates.
///
/// The order is the point, so the three phases are declared in the order §9's
/// six steps list them, and no step's check is left to the final suite:
///
/// - [`Phase::Red`] holds the agent to the test paths and runs the targeted
///   check, because the runner's job here is to confirm the *new* failure.
/// - [`Phase::Green`] opens the tree up and runs the same check again, this
///   time expecting the test that just failed to pass.
/// - [`Phase::Refactor`] keeps that same check green while cleanup happens.
///
/// Red and green record evidence, which is the only way §9's "no final test run
/// can prove tests were written first" is worth anything.
#[must_use]
pub fn tdd() -> Protocol {
    assemble(
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
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every protocol v1 can build.
    ///
    /// The ledger the two constitution tests below walk: a third constructor —
    /// §9's `spec-first`, which is v0.2 — has to be added here as well as
    /// there, because "ends with Verify then Publish" is a rule about every
    /// protocol and not a fact about the two that exist today.
    fn every_protocol() -> Vec<Protocol> {
        vec![direct(), tdd()]
    }

    /// The phases of `protocol`, as [`Phase`] values, for a comparison that
    /// reads as a sequence rather than as a list of structs.
    fn sequence(protocol: &Protocol) -> Vec<Phase> {
        protocol.phases.iter().map(|spec| spec.phase).collect()
    }

    /// The declaration of `phase` in `protocol`, which must exist.
    fn spec(protocol: &Protocol, phase: Phase) -> &PhaseSpec {
        protocol
            .phases
            .iter()
            .find(|spec| spec.phase == phase)
            .unwrap_or_else(|| panic!("`{}` declares no {phase:?}", protocol.name))
    }

    #[test]
    fn every_protocol_ends_with_verify_then_publish() {
        for protocol in every_protocol() {
            let phases = sequence(&protocol);
            let count = phases.len();
            assert!(
                count >= 2,
                "`{}` has {count} phases, too few to end with both completion phases: {phases:?}",
                protocol.name,
            );
            assert_eq!(
                &phases[count - 2..],
                [Phase::Verify, Phase::Publish],
                "`{}` must finish with the two mandatory phases, in that order; it ends {phases:?}",
                protocol.name,
            );
        }
    }

    #[test]
    fn a_body_cannot_be_a_protocol_without_the_completion_phases() {
        let protocol = assemble("empty", Vec::new());
        assert_eq!(
            sequence(&protocol),
            [Phase::Verify, Phase::Publish],
            "the ending is what a protocol is owed, whatever body it was handed",
        );
    }

    #[test]
    fn the_completion_phases_leave_the_tree_alone() {
        for protocol in every_protocol() {
            for phase in [Phase::Verify, Phase::Publish] {
                assert_eq!(
                    spec(&protocol, phase).write_scope,
                    WriteScope::None,
                    "`{}` lets {phase:?} edit the tree it is about to be judged on or published",
                    protocol.name,
                );
            }
        }
    }

    #[test]
    fn the_verify_phase_declares_the_mandatory_suite() {
        for protocol in every_protocol() {
            assert_eq!(
                spec(&protocol, Phase::Verify).gate,
                Some(GateKind::Verify),
                "`{}` reaches completion without the suite §8 calls not optional",
                protocol.name,
            );
        }
    }

    #[test]
    fn publication_is_proven_by_evidence_because_no_gate_spells_it() {
        for protocol in every_protocol() {
            let publish = spec(&protocol, Phase::Publish);
            assert_eq!(
                publish.gate, None,
                "`{}` declares a gate for publication, and `GateKind` has no publication gate to declare",
                protocol.name,
            );
            assert!(
                publish.records_evidence,
                "`{}`'s publication leaves nothing behind, so nothing proves the remote holds the commit",
                protocol.name,
            );
        }
    }

    #[test]
    fn only_the_red_phase_is_held_to_the_test_paths() {
        let protocol = tdd();
        let red = spec(&protocol, Phase::Red);
        assert_eq!(
            red.write_scope,
            WriteScope::TestsOnly,
            "red may edit tests only, or the protocol cannot claim they came first",
        );
        for protocol in every_protocol() {
            let held = protocol
                .phases
                .iter()
                .filter(|spec| spec.write_scope == WriteScope::TestsOnly)
                .map(|spec| spec.phase)
                .collect::<Vec<_>>();
            assert!(
                held.iter().all(|phase| *phase == Phase::Red),
                "`{}` grants a test-only scope to {held:?}, and only red is a tests-only phase",
                protocol.name,
            );
        }
    }

    #[test]
    fn red_and_green_are_the_phases_that_record_their_own_evidence() {
        let tdd = tdd();
        for phase in [Phase::Red, Phase::Green] {
            assert!(
                spec(&tdd, phase).records_evidence,
                "{phase:?} records nothing, so §9's claim that the tests were written first is unverifiable",
            );
        }
        assert!(
            !spec(&tdd, Phase::Refactor).records_evidence,
            "refactor's claim is that the targeted gate stayed green, which the gate journals",
        );
        assert!(
            !spec(&direct(), Phase::Implement).records_evidence,
            "direct's implementation is proved by its gate, so a second artifact would be a copy",
        );
    }

    #[test]
    fn every_protocol_is_checked_and_leaves_evidence_behind() {
        for protocol in every_protocol() {
            assert!(
                protocol.phases.iter().any(|spec| spec.gate.is_some()),
                "`{}` declares no mechanical check at all",
                protocol.name,
            );
            assert!(
                protocol.phases.iter().any(|spec| spec.records_evidence),
                "`{}` records no evidence, so its attempt rests on the agent's word",
                protocol.name,
            );
        }
    }

    #[test]
    fn the_two_names_are_the_two_words_a_configuration_may_write() {
        let names = every_protocol()
            .iter()
            .map(|protocol| protocol.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["direct", "tdd"],
            "these names are what `default_protocol` holds and what `AttemptStarted` journals",
        );
    }

    #[test]
    fn direct_is_one_implementation_phase_then_the_completion_pair() {
        assert_eq!(
            direct(),
            Protocol {
                name: "direct",
                phases: vec![
                    PhaseSpec {
                        phase: Phase::Implement,
                        write_scope: WriteScope::All,
                        gate: Some(GateKind::Targeted),
                        records_evidence: false,
                    },
                    PhaseSpec {
                        phase: Phase::Verify,
                        write_scope: WriteScope::None,
                        gate: Some(GateKind::Verify),
                        records_evidence: false,
                    },
                    PhaseSpec {
                        phase: Phase::Publish,
                        write_scope: WriteScope::None,
                        gate: None,
                        records_evidence: true,
                    },
                ],
            },
        );
    }

    #[test]
    fn tdd_is_red_then_green_then_refactor_then_the_completion_pair() {
        assert_eq!(
            tdd(),
            Protocol {
                name: "tdd",
                phases: vec![
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
                        records_evidence: false,
                    },
                    PhaseSpec {
                        phase: Phase::Publish,
                        write_scope: WriteScope::None,
                        gate: None,
                        records_evidence: true,
                    },
                ],
            },
        );
    }
}

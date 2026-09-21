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
//! # Where a task's protocol comes from
//!
//! §9 chooses a protocol *per task*, so a declaration has to be reachable by a
//! word. [`for_task`] answers that question: the word a task's `**Protocol:**`
//! section holds wins, else the project's [`Config::default_protocol`], else
//! [`direct`] — and a word that names none of them is refused rather than
//! quietly worked. The names, the constructors they select and the sentence
//! that refuses an unknown one are spelled once, in `PROTOCOLS` below, so a
//! refusal cannot promise a protocol this build does not have. Choosing stays
//! declaration and not I/O: the two words are fields of a [`Task`] and a
//! [`Config`] the caller is already holding, and the check that an unknown word
//! is refused *when the task is added* lives beside the names it validates
//! (ADR-0070).
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

use crate::config::Config;
use crate::error::{Error, Result};
use crate::gate::GateKind;
use crate::state::Phase;
use crate::task::Task;

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

/// One protocol's word and the constructor that builds it from nothing.
type Named = (&'static str, fn() -> Protocol);

/// The two protocols v1 ships, each under the word that selects it.
///
/// The one place the two names are spelled. [`by_name`] resolves a word a task
/// or a configuration wrote, [`names`] lists the words a refusal offers, and
/// the two constructors are reached through this list rather than named at each
/// call site — so the word an operator may write, the phases a run walks and
/// the sentence that refuses a word nobody runs cannot drift apart.
const PROTOCOLS: [Named; 2] = [("direct", direct), ("tdd", tdd)];

/// The words a task's `**Protocol:**` section and a project's
/// [`Config::default_protocol`] may hold.
#[must_use]
pub(crate) fn names() -> [&'static str; 2] {
    PROTOCOLS.map(|(name, _build)| name)
}

/// The v1 names, quoted and joined by `separator`, for a sentence that offers
/// them: `refusal` joins them with `and`, [`crate::task`] offers them with `or`.
pub(crate) fn alternatives(separator: &str) -> String {
    names()
        .into_iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(separator)
}

/// The protocol a word selects, matched exactly.
///
/// No folding, no trimming, no prefix: a word that had to be repaired to match
/// is a word the operator wrote wrong, and running a task under a protocol they
/// did not ask for is the cheaper-looking mistake this refuses to make.
pub(crate) fn by_name(name: &str) -> Option<Protocol> {
    PROTOCOLS
        .into_iter()
        .find(|(word, _build)| *word == name)
        .map(|(_word, build)| build())
}

/// The sentence a word that names no protocol is refused by — written here so
/// the import that rejects a task file, the lint that rejects a row and the run
/// that refuses an attempt all refuse it in the same words.
pub(crate) fn refusal(name: &str) -> String {
    format!(
        "`{name}` is not a work protocol this build runs; the two it has are {}",
        alternatives(" and ")
    )
}

/// The protocol `task` is worked with: the word its own `**Protocol:**` section
/// holds, else the project's [`Config::default_protocol`], else [`direct`].
///
/// The order is the whole of the function, and it is §9's: "Protocols are chosen
/// per task", and a project "defaults to" one. A task's word wins over the
/// project's, and the project's word wins over the compiled-in `direct` — which
/// is the last rung rather than another setting, because a queue with nothing
/// written anywhere still has to be worked, and §9 calls `direct` the v0.1
/// default.
///
/// The returned [`Protocol::name`] is the word [`crate::EventKind::AttemptStarted`]
/// journals for the attempt about to start: an attempt that did not record which
/// protocol it ran under cannot be replayed, and a `PhaseEntered` naming a phase
/// no protocol declared is unfalsifiable without the protocol beside it.
///
/// A blank is no word: a section, or a setting, with nothing written in it names
/// nothing and falls through to the rung below it rather than being read as a
/// choice of `direct`. An *empty* `**Protocol:**` section is refused earlier, by
/// [`crate::validate`] when the task is added; a blank reaching here is a row
/// assembled in memory, which is answered the same way an unset default is.
///
/// # Errors
///
/// [`Error::Config`] keyed `protocol` when the task's own word names no protocol
/// this build runs, and keyed `default_protocol` when the project's does — naming
/// the word that was refused and the two that would have worked. A name should
/// have been refused when the task was added ([`crate::validate`] asks it of
/// every row); reaching here with one means the row was written by something that
/// did not ask, and the attempt is refused rather than quietly worked `direct`.
pub fn for_task(task: &Task, config: &Config) -> Result<Protocol> {
    if let Some(name) = task
        .protocol
        .as_deref()
        .map(str::trim)
        .filter(|word| !word.is_empty())
    {
        return by_name(name).ok_or_else(|| Error::Config {
            key: "protocol".to_owned(),
            detail: refusal(name),
        });
    }
    let default = config.default_protocol.trim();
    if default.is_empty() {
        return Ok(direct());
    }
    by_name(default).ok_or_else(|| Error::Config {
        key: "default_protocol".to_owned(),
        detail: refusal(default),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, Config, Error, EventKind, Task, TaskStatus};
    use serde_json::Value;

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

    /// A parsed queue task declaring `**Protocol:**` `name`, or naming none.
    ///
    /// Built by [`crate::parse_plan`] rather than by a struct literal, because a
    /// queue row is what a run is handed and a protocol chosen against anything
    /// else is chosen against a shape the queue never holds.
    fn task_declaring(name: Option<&str>) -> Task {
        let mut document = "\
## T075 Choose the protocol for one task

**Outcome:** a task's protocol is chosen before its first attempt.
**Done-when:** the choice is stored and displayed.
**Verify:** `cargo nextest run -p ktask-core`
**Refs:** VISION.md section 9
"
        .to_owned();
        if let Some(protocol) = name {
            document.push_str("**Protocol:** ");
            document.push_str(protocol);
            document.push('\n');
        }
        let mut tasks = crate::parse_plan(&document)
            .expect("a task that names a protocol it knows is a well-formed task");
        tasks.remove(0)
    }

    /// The same task, holding `name` as its protocol whatever the word is — the
    /// shape only a row written by some other build could reach, and the one a
    /// refusal has to survive.
    fn task_holding(name: &str) -> Task {
        Task {
            protocol: Some(name.to_owned()),
            ..task_declaring(None)
        }
    }

    /// A project's configuration, with only its default protocol changed.
    ///
    /// Assigned rather than built with `..Config::default()`: a `Config` holds a
    /// private provenance map, so struct-update syntax is closed outside
    /// `config.rs` — which is the right rule, since a hand-built configuration
    /// should record nothing it did not load.
    fn configured(default: &str) -> Config {
        let mut config = Config::default();
        config.default_protocol = default.to_owned();
        config
    }

    #[test]
    fn a_task_that_names_tdd_is_worked_with_the_tdd_phases() {
        let chosen = for_task(&task_declaring(Some("tdd")), &Config::default())
            .expect("`tdd` is a protocol this build runs");
        assert_eq!(
            chosen,
            tdd(),
            "the word a task writes selects the phases §9 declares under that word, and nothing else",
        );
        assert_eq!(chosen.name, "tdd");
    }

    #[test]
    fn a_task_that_names_direct_is_worked_with_the_direct_phases() {
        let chosen = for_task(&task_declaring(Some("direct")), &Config::default())
            .expect("`direct` is a protocol this build runs");
        assert_eq!(chosen, direct());
        assert_eq!(chosen.name, "direct");
    }

    #[test]
    fn a_task_that_names_nothing_takes_the_projects_default() {
        let chosen = for_task(&task_declaring(None), &configured("tdd"))
            .expect("a project may default its queue to `tdd`");
        assert_eq!(
            chosen,
            tdd(),
            "a task that names no protocol is worked the way its project says, not the way \
             this module happens to prefer",
        );
    }

    #[test]
    fn the_task_is_asked_before_the_project() {
        let chosen = for_task(&task_declaring(Some("direct")), &configured("tdd"))
            .expect("the task's own word is a protocol this build runs");
        assert_eq!(
            chosen,
            direct(),
            "`default_protocol` is a default, so a task that names `direct` is worked `direct` \
             however its project is configured",
        );
    }

    #[test]
    fn a_project_that_writes_no_default_is_worked_direct() {
        let chosen = for_task(&task_declaring(None), &configured("   "))
            .expect("a default with nothing written in it is no default at all");
        assert_eq!(
            chosen,
            direct(),
            "`direct` is the last rung of the chain and the one §9 calls the v0.1 default, so \
             a task with nothing to ask either way is worked that way",
        );
    }

    #[test]
    fn a_task_naming_a_protocol_nobody_runs_is_refused_before_an_attempt_starts() {
        let error = for_task(&task_holding("spec-first"), &Config::default()).expect_err(
            "a word that names no phases cannot be run, and must not be run as `direct` instead",
        );
        let Error::Config { key, detail } = error else {
            panic!("an unusable protocol word is a configuration error, not {error}");
        };
        assert_eq!(
            key, "protocol",
            "the refusal names the section the word came from"
        );
        assert!(
            detail.contains("spec-first") && detail.contains("direct") && detail.contains("tdd"),
            "the refusal has to quote the word it refused and the words that would have \
             worked: {detail}"
        );
    }

    #[test]
    fn a_default_that_names_nothing_runnable_is_refused_rather_than_run_as_direct() {
        let error = for_task(&task_declaring(None), &configured("tdd-v2"))
            .expect_err("a misspelt default is not the same instruction as no instruction");
        let Error::Config { key, detail } = error else {
            panic!("an unusable `default_protocol` is a configuration error, not {error}");
        };
        assert_eq!(
            key, "default_protocol",
            "the refusal names the setting the operator has to fix, not the task that tripped on it"
        );
        assert!(
            detail.contains("tdd-v2") && detail.contains("direct") && detail.contains("tdd"),
            "the refusal has to quote the word it refused and the words that would have \
             worked: {detail}"
        );
    }

    #[test]
    fn the_word_attempt_started_journals_names_the_protocol_that_was_chosen() {
        for name in ["direct", "tdd"] {
            let chosen = for_task(&task_declaring(Some(name)), &Config::default())
                .expect("both v1 words name a protocol this build runs");
            let event = EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: chosen.name.to_owned(),
                pid: 4242,
                base_sha: "0b78d3f1c2a4b5e6d7f8091a2b3c4d5e6f708192".to_owned(),
            };
            let encoded = serde_json::to_value(&event).expect("a journal record encodes");
            assert_eq!(
                encoded.get("protocol").and_then(Value::as_str),
                Some(name),
                "`AttemptStarted` has to carry the word the task was worked under, or the \
                 journal records an attempt with no protocol",
            );
            let decoded: EventKind = serde_json::from_value(encoded)
                .expect("the record a run writes is one it can read back");
            let EventKind::AttemptStarted { protocol, .. } = decoded else {
                panic!("the record written was an `AttemptStarted`");
            };
            assert_eq!(
                by_name(&protocol).expect("the journaled word names a protocol"),
                chosen,
                "the word in the journal has to rebuild the phases the attempt ran, or a \
                 replay cannot say what an attempt did",
            );
        }
    }

    #[test]
    fn the_names_a_word_may_hold_are_the_names_the_constructors_carry() {
        // `by_name`, the refusal's sentence and `default_protocol` all speak
        // this list, so the test that pins the constructors' names pins the
        // words a task and a configuration are allowed to write.
        let words = names();
        assert_eq!(words, ["direct", "tdd"]);
        for protocol in every_protocol() {
            assert_eq!(
                by_name(protocol.name).expect("a constructor's own name selects it"),
                protocol,
                "`{}` is not reachable by the word it carries",
                protocol.name,
            );
        }
        assert!(
            by_name("Direct").is_none() && by_name(" TDD ").is_none() && by_name("").is_none(),
            "the word is matched exactly: a name that needed folding or trimming is a word \
             the operator wrote wrong, not a near miss",
        );
    }

    #[test]
    fn a_protocol_field_with_nothing_written_in_it_is_no_word_at_all() {
        // Only a row assembled in memory can hold this: an empty
        // `**Protocol:**` section is refused when the task is added. A blank is
        // read as the absence it is, so the project's default answers rather
        // than the task being run `direct` because a field was left half
        // written.
        let mut task = task_declaring(None);
        task.protocol = Some("   ".to_owned());
        assert_eq!(
            for_task(&task, &configured("tdd"))
                .expect("a blank names nothing, so the default answers"),
            tdd(),
        );
    }

    #[test]
    fn a_task_built_by_hand_still_answers_for_its_protocol() {
        // The queue's own task, held without a body: the choice is a field, so
        // a caller that assembled the row rather than parsing it is asked the
        // same question.
        let mut task = task_declaring(None);
        task.protocol = Some("tdd".to_owned());
        task.status = TaskStatus::Pending;
        assert_eq!(
            for_task(&task, &Config::default()).expect("`tdd` is a protocol this build runs"),
            tdd(),
        );
    }
}

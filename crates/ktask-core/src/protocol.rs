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
//!
//! # How a phase's scope is enforced
//!
//! A phase's [`PhaseSpec::write_scope`] is a declaration; [`check_scope`] is the
//! enforcement, and it is what makes §9's "the agent may add or modify tests
//! only" a rule rather than a sentence in a prompt. It is handed the paths the
//! *repository* says changed — the list [`crate::git::changed_paths`] reads out
//! of `git diff` and the untracked listing — because the alternative is the
//! agent's own account of what it edited, and §3's invariant 4 is precisely a
//! refusal to take a run's evidence from the party being graded.
//!
//! It stays a pure predicate, like the rest of the module: no `git`, no
//! filesystem, no clock. It resolves a scope against a diff and answers with a
//! [`crate::Error::Policy`] or with nothing, which is what makes the two cases
//! §9 turns on testable without a repository to dirty — a production file
//! touched during red, and a verify phase that touched nothing.
//!
//! [`crate::check_no_policy_edit`] is the other check on the same diff and not a
//! rival: a scope says which phase may write where, and that one says no phase
//! may write the rules the run is judged by, whichever scope it held.
//!
//! # How red is confirmed
//!
//! A scope says what an agent may write; §9's claim about red is a claim about
//! *time* — the test failed before the implementation existed — and no rule
//! about paths can prove that. [`verify_red`] is that half. The runner runs the
//! phase's targeted check twice, once before the agent works and once after, and
//! the difference between the two failure lists is the only evidence that
//! survives the phase: a failure set that did not change is refused, so "the
//! suite was already red" cannot be filed as proof a test came first, and a red
//! phase that ended green means precisely what it looks like.
//!
//! # How green is confirmed
//!
//! §9 step 4 hands the runner the other half: "The runner confirms the new test
//! passes." [`verify_green`] is handed the names [`verify_red`] returned and the
//! summary of one re-run of that same targeted command, and it refuses two
//! different claims. A named test the run still lists as failing is the phase
//! ending where red ended. A test the run lists as failing that nobody named is
//! the one that ends a task quietly, because [`Phase::Green`] holds every path
//! open and an edit that broke an unrelated test looks, from outside the phase,
//! exactly like an edit that fixed one — so a regression elsewhere is refused,
//! by name, rather than being left for the final verification gate to find after
//! the attempt was already called good.
//!
//! The two halves differ in one way that is not symmetric. Red's evidence is a
//! *difference* between two runs and so needs both; green's is a property of one
//! run, because the phase's own red run is what fixed which tests were passing
//! when it started. That is also the whole of its weakness: [`TestSummary`] lists
//! what failed and never what passed, so a test that vanished from the run —
//! renamed, deleted, ignored — vanishes from the failure list too, and only the
//! run's own passing count can say the run was too small to have confirmed
//! anything. [`verify_green`] reads that count as a floor for exactly that
//! reason, and says in the open what the floor does not reach.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::gate::{GateKind, TestSummary};
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

/// The sentence a refusal quotes when a phase wrote a path its scope did not
/// grant.
const SCOPE_RULE: &str = "a phase may not write outside the paths its write scope declares";

/// The sentence a refusal quotes when a changed path cannot be placed inside the
/// worktree the scope is drawn on — the one case this check cannot judge, and so
/// it is refused rather than trusted. See [`check_scope`].
const UNLOCATABLE_RULE: &str =
    "a changed path that cannot be located inside the worktree is refused rather than trusted";

/// The one pattern that stands for a whole path segment, or for any number of
/// them. Anywhere else — `**tests.rs` — it is two ordinary `*`.
const DOUBLE_STAR: &str = "**";

/// Refuse a phase whose diff wrote outside the paths its scope grants.
///
/// `changed` is what the repository says the phase touched: the paths
/// [`crate::git::changed_paths`] reads out of `git diff` and the untracked
/// listing, repository-relative and in path order. It is never the agent's list.
/// §9's "the agent may add or modify tests only" is a rule about files, and a
/// rule about files that is checked against the account of the party being
/// graded is a sentence in a prompt.
///
/// The three scopes answer three questions:
///
/// - [`WriteScope::All`] grants the worktree. `direct`'s implement phase and
///   `tdd`'s green and refactor phases run under it, and it is no grant over
///   anything outside the worktree: a path that cannot be placed inside it is
///   refused here too.
/// - [`WriteScope::TestsOnly`] grants exactly the paths `test_globs` names and
///   nothing else — production code, and with it the gate configuration the run
///   is judged by. This is red's scope, and the reason §9's claim that the tests
///   came first is checkable rather than claimed.
/// - [`WriteScope::None`] grants nothing: every changed path is an offender,
///   which is §10's "dirty tree at verification time" arriving as a policy
///   failure instead of as a suite run against an uncommitted tree. An empty
///   diff passes, because a phase that changed nothing has nothing to refuse.
///
/// `test_globs` is the project's [`Config::test_globs`] — the language
/// profile's, not a list compiled in beside the protocol (ADR-0068). One
/// protocol means the same thing in a Rust tree and a TypeScript tree because a
/// phase declares the *kind* of access it grants and the project supplies the
/// paths. An empty list makes `TestsOnly` grant nothing, which is the safe
/// reading of a project that never said where its tests live: the alternative is
/// a red phase writing wherever a guessed layout pointed.
///
/// # What a glob matches
///
/// Matching is on a path's components, so no pattern crosses a directory
/// boundary by accident:
///
/// | pattern | matches |
/// |---|---|
/// | `*` | any run of characters inside one path segment, and no separator |
/// | `?` | exactly one character inside one path segment |
/// | `**` | only as a whole segment: no directory, or this one and every directory below it |
/// | any other character | itself, `.` and `-` included |
///
/// A pattern is anchored where it is written, so `src/**/tests.rs` matches
/// `src/tests.rs` and `src/store/tests.rs` and not a root-level `tests.rs`.
/// Braces and bracketed classes are not a glob language here: `*.{rs,test}`
/// matches a file whose name ends in `{rs,test}`, which is a refusal an operator
/// notices rather than a scope that quietly widened past what was written. A
/// path that is not valid UTF-8 matches no glob, a glob being text, so a
/// `TestsOnly` phase cannot write one and every other scope treats it as the
/// ordinary path it is.
///
/// # Errors
///
/// [`Error::Policy`] quoting the rule broken — both sentences when a diff broke
/// two — and naming every offending path in the order `changed` listed them, so
/// the inspector and the failure bundle send a human to the files that broke the
/// scope and to no others. The classifier already reads a policy error as
/// [`FailureClass::PolicyFailure`](crate::FailureClass::PolicyFailure), which is
/// §7's "forbidden file, dirty tree, attempted gate bypass" and earns no retry:
/// an agent cannot repair a scope violation by editing more files.
pub fn check_scope(scope: WriteScope, changed: &[PathBuf], test_globs: &[String]) -> Result<()> {
    let mut offenders = Vec::new();
    let mut outside = false;
    let mut unlocatable = false;
    for path in changed {
        let Some(parts) = inside_worktree(path) else {
            unlocatable = true;
            offenders.push(path.clone());
            continue;
        };
        if writes_outside(scope, &parts, test_globs) {
            outside = true;
            offenders.push(path.clone());
        }
    }
    if offenders.is_empty() {
        return Ok(());
    }
    let mut rules = Vec::with_capacity(2);
    if outside {
        rules.push(SCOPE_RULE);
    }
    if unlocatable {
        rules.push(UNLOCATABLE_RULE);
    }
    Err(Error::Policy {
        detail: rules.join("; "),
        paths: offenders,
    })
}

/// The components of `path`, as far as they lie inside the worktree.
///
/// `.` is dropped and `..` walks back up the components it was given, so
/// `src/../tests/mod.rs` is read as the test path it names. A path this cannot
/// place — absolute, or climbing above the root the scope is drawn on — answers
/// [`None`] rather than a guess, because a check that passed what it could not
/// read would let `../elsewhere/tests/mod_test.rs` into a red phase on the
/// strength of a name that happens to match a test glob.
fn inside_worktree(path: &Path) -> Option<Vec<&OsStr>> {
    let mut parts: Vec<&OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then_some(parts)
}

/// Whether `scope` forbids a write to the path whose components are `parts`.
///
/// `test_globs` is asked of [`WriteScope::TestsOnly`] alone: `All` grants the
/// whole worktree whatever its shape, and `None` grants so little that the shape
/// does not matter.
fn writes_outside(scope: WriteScope, parts: &[&OsStr], test_globs: &[String]) -> bool {
    match scope {
        WriteScope::All => false,
        WriteScope::None => true,
        WriteScope::TestsOnly => !matches_a_glob(parts, test_globs),
    }
}

/// Whether any of `test_globs` names the path whose components are `parts`.
fn matches_a_glob(parts: &[&OsStr], test_globs: &[String]) -> bool {
    test_globs.iter().any(|glob| matches_glob(glob, parts))
}

/// Whether `glob` names the path whose components are `parts`.
fn matches_glob(glob: &str, parts: &[&OsStr]) -> bool {
    let segments: Vec<&str> = parts.iter().filter_map(|part| part.to_str()).collect();
    if segments.len() != parts.len() {
        return false;
    }
    match_segments(&glob.split('/').collect::<Vec<_>>(), &segments)
}

/// Whether `pattern`'s segment patterns match `segments`, from the front of each.
fn match_segments(pattern: &[&str], segments: &[&str]) -> bool {
    match pattern {
        [] => segments.is_empty(),
        [DOUBLE_STAR, deeper @ ..] => {
            match_segments(deeper, segments)
                || segments
                    .split_first()
                    .is_some_and(|(_head, below)| match_segments(pattern, below))
        }
        [word, deeper @ ..] => segments
            .split_first()
            .is_some_and(|(head, below)| match_name(word, head) && match_segments(deeper, below)),
    }
}

/// Whether the one segment pattern `word` matches the one path segment `name`.
fn match_name(word: &str, name: &str) -> bool {
    match_name_chars(
        &word.chars().collect::<Vec<_>>(),
        &name.chars().collect::<Vec<_>>(),
    )
}

/// Whether `pattern` matches `name` character for character, `*` and `?` aside.
fn match_name_chars(pattern: &[char], name: &[char]) -> bool {
    match pattern {
        [] => name.is_empty(),
        ['*', rest @ ..] => {
            match_name_chars(rest, name)
                || name
                    .split_first()
                    .is_some_and(|(_head, below)| match_name_chars(pattern, below))
        }
        [first, rest @ ..] => name.split_first().is_some_and(|(head, below)| {
            (*head == *first || *first == '?') && match_name_chars(rest, below)
        }),
    }
}

/// The sentence a red phase is refused by when its run holds no new failure.
const RED_RULE: &str =
    "a red phase has to leave a test that failed after the change and did not fail before it";

/// Confirm a red phase left a genuinely new failing test, and name it.
///
/// VISION.md §9's step 2 belongs to the runner, not the agent: "The runner
/// executes `targeted_test_command` and confirms the expected *new* failure."
/// Two runs of that one command answer the question — `before` the run the phase
/// started from, `after` the run that ended it — and only a comparison of the two
/// separates a test written to fail from a suite that was already red. An
/// unchanged failure set is refused for that reason: an agent that edited
/// nothing that fails, added a test that passes, or repaired an old failure all
/// look identical at the end of the phase, which is exactly what §9 refuses to
/// take on faith.
///
/// The answer is a list of names rather than a verdict, because names are what
/// the rest of the protocol acts on: [`tdd`]'s [`Phase::Green`] re-runs the tests
/// that just failed, in these words, to prove they pass now, and §9's "RED and
/// GREEN evidence (command, output, tree hash) is stored with the attempt" files
/// them as red's half of that record — which is why that phase declares
/// [`PhaseSpec::records_evidence`].
///
/// # Names, not counts
///
/// The difference is taken over [`TestSummary::failures`] and not
/// [`TestSummary::failed`], because a count moves for reasons that have nothing
/// to do with newness: repairing one old failure lowers it while adding no
/// evidence, and renaming a failing test raises a name nobody has read while the
/// failure itself stays as old as the baseline. The list is complete rather than
/// suggestive because [`crate::parse_cargo`] refuses output whose `failures:`
/// block holds a different number of names than that output's own line counted —
/// a summary that exists at all names every test that failed.
///
/// A name two test binaries both reported is answered once, at the position the
/// run first wrote it: one test is one thing to prove in green, and evidence
/// naming it twice would be read as two obligations.
///
/// # Errors
///
/// [`Error::Gate`] naming [`GateKind::Targeted`] — the gate [`tdd`]'s red phase
/// declares — when nothing failed newly. The sentence quotes the rule and both
/// runs' lists, so the inspector and the failure bundle show an operator the
/// comparison that refused instead of a bare "failed". The classifier gives that
/// variant no class of its own (ADR-0059), so an attempt refused here lands as
/// [`FailureClass::AgentFailure`](crate::FailureClass::AgentFailure) unless the
/// caller's own gate list argues otherwise — and that is the right landing: a
/// bounded fresh session can answer this refusal by writing a test that really
/// fails, which is why it is not an [`Error::Policy`], a class that earns no
/// retry.
///
/// Like the rest of this module this holds no I/O and no clock: the two summaries
/// are the ones a caller got by running the gate twice. Nothing calls it yet, as
/// with [`check_scope`] — the runner that walks a protocol's phases is the task
/// that wires both.
pub fn verify_red(before: &TestSummary, after: &TestSummary) -> Result<Vec<String>> {
    let already_failing = before
        .failures
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut named = HashSet::new();
    let mut fresh = Vec::new();
    for name in &after.failures {
        if already_failing.contains(name.as_str()) || !named.insert(name.as_str()) {
            continue;
        }
        fresh.push(name.clone());
    }
    if fresh.is_empty() {
        return Err(Error::Gate {
            kind: GateKind::Targeted.to_string(),
            detail: format!(
                "{RED_RULE}; the run found none (failing before: {}; failing after: {})",
                listed(&before.failures),
                listed(&after.failures)
            ),
        });
    }
    Ok(fresh)
}

/// The sentence a green phase is refused by when its run cannot confirm it.
const GREEN_RULE: &str = "a green phase has to leave every test the red phase named passing and \
                          no test that was passing before it failing";

/// Confirm a green phase passed the tests red made it fail and broke nothing else.
///
/// VISION.md §9 step 4 is the runner's, like step 2: once green opens the
/// implementation, "the runner confirms the new test passes." `expected` is what
/// [`verify_red`] returned for the phase — the names that failed and did not fail
/// before it — and `after` is the summary of one run of the *same*
/// `targeted_test_command` the red phase ran. Two claims come out of those two
/// arguments, and the phase is refused when either of them fails.
///
/// **The named tests have to pass.** A name `expected` holds that
/// [`TestSummary::failures`] still lists is the test the phase existed for, still
/// failing: green ended where red ended.
///
/// **Nothing else may start failing.** [`Phase::Green`] takes
/// [`WriteScope::All`] — the implementation is what the phase was for — so the run
/// answers for everything the command covers, not only for its own test. A name
/// the run lists that `expected` does not was not failing when the phase started,
/// because [`verify_red`] handed over every name that newly failed and green is
/// re-running that same list; it is failing now, which is a regression, and it is
/// refused by name. "The new test passes" is worth nothing if the sentence has to
/// end "...and four others no longer do," and an edit that broke an unrelated test
/// looks, from the outside, exactly like an edit that fixed one.
///
/// # Absence is the signal, and how far it reaches
///
/// [`TestSummary`] holds counts and a failure list and no list of the tests that
/// passed, so a name is confirmed to pass by not appearing in
/// [`TestSummary::failures`]. That is sound rather than hopeful because of what
/// [`crate::parse_cargo`] refuses (ADR-0039): output whose `failures:` block names
/// fewer tests than its own result line counted, and a transcript that opened a
/// test binary and never wrote its count. A summary that exists at all therefore
/// names every test that failed, so everything the run ran is either listed here
/// or passed.
///
/// What is left is a test the run never ran at all: renamed out of the filter,
/// deleted, or given an `#[ignore]`, it vanishes from the failure list without
/// passing. ADR-0072 named that a third answer needing its own ruling, and the
/// ruling is the run's own `passed` count read as a floor — `k` distinct names
/// cannot all have passed in a run that reported fewer than `k` passing tests, so
/// the empty re-run a deleted test leaves behind is refused rather than filed as
/// the evidence §9 wants. It is a floor and not a proof: a run with a test
/// swallowed by a *larger* population still reads as green here. The rest of that
/// door is outside this predicate — the re-run has to be the same command red ran,
/// since a wider or narrower filter changes what "was passing before" means, and
/// §9 step 6's full verification runs the whole suite after the phase that could
/// have shrunk it.
///
/// # Errors
///
/// [`Error::Gate`] naming [`GateKind::Targeted`] — the gate [`tdd`]'s green phase
/// declares — quotes the rule and then the names in both buckets (a name
/// two binaries both reported is named once, as in [`verify_red`], because one
/// broken test is one thing to report), or the passing count against the names it
/// fell short of. Like [`verify_red`] this is a gate error and not a
/// [`Error::Policy`]: `classify` gives a gate error no class of its own (ADR-0059),
/// so the attempt lands as an
/// [`FailureClass::AgentFailure`](crate::FailureClass::AgentFailure) and earns the
/// bounded fresh session that can repair a regression, which a policy failure
/// would not.
///
/// Nothing calls it yet, as with [`check_scope`] and [`verify_red`]: the runner
/// that walks a protocol's phases wires all three, and files what this refuses
/// beside the gate log ADR-0065 already gives the attempt — which is where
/// §9's GREEN evidence (command, output, tree hash) comes from, and why that phase
/// declares [`PhaseSpec::records_evidence`].
pub fn verify_green(expected: &[String], after: &TestSummary) -> Result<()> {
    let mut named = HashSet::new();
    let mut distinct = Vec::new();
    for name in expected {
        if named.insert(name.as_str()) {
            distinct.push(name.clone());
        }
    }

    let mut seen = HashSet::new();
    let mut still = Vec::new();
    let mut regressed = Vec::new();
    for name in &after.failures {
        if !seen.insert(name.as_str()) {
            continue;
        }
        if named.contains(name.as_str()) {
            still.push(name.clone());
        } else {
            regressed.push(name.clone());
        }
    }
    if !still.is_empty() || !regressed.is_empty() {
        return Err(Error::Gate {
            kind: GateKind::Targeted.to_string(),
            detail: format!(
                "{GREEN_RULE}; the run reported (expected and still failing: {}; passing before \
                 and failing now: {})",
                listed(&still),
                listed(&regressed)
            ),
        });
    }

    let wanted = u32::try_from(distinct.len()).unwrap_or(u32::MAX);
    if wanted > after.passed {
        return Err(Error::Gate {
            kind: GateKind::Targeted.to_string(),
            detail: format!(
                "{GREEN_RULE}; the run reported {} passing tests against the {} named to be \
                 confirmed ({}), so at least one of them did not pass",
                after.passed,
                wanted,
                listed(&distinct)
            ),
        });
    }
    Ok(())
}

/// The tests one summary named as failing, written for the refusal that quotes
/// them — and worded so that a run with no failing test says `nothing` rather
/// than leaving the reader to read an empty pair of brackets as a bug.
fn listed(failures: &[String]) -> String {
    if failures.is_empty() {
        return "nothing".to_owned();
    }
    failures
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, Config, Error, EventKind, Task, TaskStatus};
    use serde_json::Value;
    use std::path::PathBuf;

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

    /// The paths a Rust project's `test_globs` name, as `Config` ships them.
    fn rust_globs() -> Vec<String> {
        Config::default().test_globs
    }

    /// A language profile's globs, as the configuration holds them.
    fn glob_list(patterns: &[&str]) -> Vec<String> {
        patterns
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect()
    }

    /// A diff of `paths`, in the order written, as [`crate::git::changed_paths`]
    /// hands one over.
    fn diff(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// Assert `scope` permits `paths`, resolved against `globs`.
    fn permitted(scope: WriteScope, paths: &[&str], globs: &[String]) {
        if let Err(error) = check_scope(scope, &diff(paths), globs) {
            panic!("{scope:?} refused {paths:?} under {globs:?}: {error}");
        }
    }

    /// The refusal `scope` handed back for `paths` resolved against `globs`, as
    /// the rule it quoted and the paths it named.
    fn refused(scope: WriteScope, paths: &[&str], globs: &[String]) -> (String, Vec<String>) {
        let error = check_scope(scope, &diff(paths), globs)
            .expect_err("the diff is one the scope is expected to refuse");
        match error {
            Error::Policy { detail, paths } => (
                detail,
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
            ),
            other => panic!("a write-scope refusal is a policy violation, not {other}"),
        }
    }

    #[test]
    fn tests_only_refuses_a_production_edit_and_names_the_path() {
        // The done-when, first half: the red phase's whole point is that this
        // edit ends the attempt rather than passing with the test suite green.
        let production = "crates/ktask-core/src/protocol.rs";
        let (detail, named) = refused(WriteScope::TestsOnly, &[production], &rust_globs());
        assert_eq!(
            named,
            [production],
            "a production edit has to be refused by name, or the refusal cannot be acted on",
        );
        assert_eq!(
            detail, SCOPE_RULE,
            "a scope refusal quotes the scope rule alone"
        );
    }

    #[test]
    fn tests_only_permits_an_edit_to_a_test_path() {
        // The done-when, second half. Four spellings a Rust project's tests
        // actually come in, each matched by a different default glob.
        permitted(
            WriteScope::TestsOnly,
            &[
                "crates/ktask-core/tests/protocol.rs",
                "tests/terminal.rs",
                "crates/ktask-core/src/gate_test.rs",
                "src/parser/tests.rs",
            ],
            &rust_globs(),
        );
    }

    #[test]
    fn a_default_glob_reaches_where_it_was_written_and_a_project_glob_reaches_further() {
        // The third shipped glob is `src/**/tests.rs`, anchored at the root like
        // every pattern that opens with a name — so in a workspace it covers a
        // top-level `src/` and no other. Refusing the deeper one is the anchor
        // working, and the fix is the project's own glob, which is exactly the
        // knob §9's per-language configuration is for.
        let nested = "crates/ktask-core/src/parser/tests.rs";
        let (_, named) = refused(WriteScope::TestsOnly, &[nested], &rust_globs());
        assert_eq!(
            named,
            [nested],
            "`src/**/tests.rs` does not start with a `**/`, so it may not reach into a crate's              own src/ and silently become a whole-tree pattern",
        );

        let widened = glob_list(&["crates/**/src/**/tests.rs"]);
        permitted(WriteScope::TestsOnly, &[nested], &widened);
    }

    #[test]
    fn tests_only_names_every_production_path_and_no_test_path() {
        let (detail, named) = refused(
            WriteScope::TestsOnly,
            &[
                "crates/ktask-core/src/protocol.rs",
                "crates/ktask-core/tests/protocol.rs",
                "docs/CONTRACT.md",
            ],
            &rust_globs(),
        );
        assert_eq!(
            named,
            ["crates/ktask-core/src/protocol.rs", "docs/CONTRACT.md"],
            "one clean path in a dirty diff cannot pay for the two that broke the scope, and              the refusal lists them in the order the diff did",
        );
        assert_eq!(detail, SCOPE_RULE);
    }

    #[test]
    fn none_refuses_a_test_edit_as_soon_as_a_production_one() {
        let (detail, named) = refused(
            WriteScope::None,
            &[
                "crates/ktask-core/tests/protocol.rs",
                "crates/ktask-cli/src/main.rs",
            ],
            &rust_globs(),
        );
        assert_eq!(
            named,
            [
                "crates/ktask-core/tests/protocol.rs",
                "crates/ktask-cli/src/main.rs"
            ],
            "verify and publish hold nothing, so a test path is as much a dirty tree as a              production one",
        );
        assert_eq!(detail, SCOPE_RULE);
    }

    #[test]
    fn none_permits_a_phase_that_changed_nothing() {
        // §10's completion phases run on a tree that is already committed, and
        // an empty diff is the case that passes: refusing it would fail every
        // honest run, and permitting a non-empty one would verify uncommitted
        // work.
        permitted(WriteScope::None, &[], &rust_globs());
    }

    #[test]
    fn all_permits_a_production_edit_and_a_test_edit_alike() {
        permitted(
            WriteScope::All,
            &[
                "crates/ktask-core/src/protocol.rs",
                "crates/ktask-core/tests/protocol.rs",
            ],
            &rust_globs(),
        );
    }

    #[test]
    fn all_is_granted_the_whole_tree_without_being_asked_what_a_test_is() {
        // `test_globs` bears on `TestsOnly` alone: an implementation phase keeps
        // its write scope even in a project that named no test path at all.
        permitted(WriteScope::All, &["crates/ktask-core/src/protocol.rs"], &[]);
    }

    #[test]
    fn tests_only_with_no_globs_written_permits_no_path_at_all() {
        let (detail, named) = refused(
            WriteScope::TestsOnly,
            &["crates/ktask-core/tests/protocol.rs"],
            &[],
        );
        assert_eq!(
            named,
            ["crates/ktask-core/tests/protocol.rs"],
            "a project that wrote no `test_globs` never said where its tests live, so red has              nowhere to write; guessing a layout would send the phase wherever the guess pointed",
        );
        assert_eq!(detail, SCOPE_RULE);
    }

    #[test]
    fn what_counts_as_a_test_is_the_projects_glob_not_a_compiled_in_layout() {
        // The done-when's third clause, both directions. The same protocol, the
        // same scope, and a different language profile moves the line.
        let typescript = glob_list(&["__tests__/**", "**/*.test.ts"]);
        permitted(
            WriteScope::TestsOnly,
            &["__tests__/auth.test.ts", "src/deep/session.test.ts"],
            &typescript,
        );
        let (_, named) = refused(WriteScope::TestsOnly, &["src/auth.ts"], &typescript);
        assert_eq!(
            named,
            ["src/auth.ts"],
            "the profile's own production code stays read-only                                            under its own globs"
        );

        let (_, named) = refused(
            WriteScope::TestsOnly,
            &["__tests__/auth.test.ts"],
            &rust_globs(),
        );
        assert_eq!(
            named,
            ["__tests__/auth.test.ts"],
            "the check has no private notion of a test path that overrides what the project              wrote: under the Rust globs that file is production",
        );
    }

    #[test]
    fn a_path_that_climbs_out_of_the_worktree_is_refused_however_it_is_named() {
        // `..` is the hole this closes: the name ends in `_test.rs` and matches
        // the default glob, so a matcher that read the string as written would
        // let the red phase edit another checkout's tests.
        let climbing = "../elsewhere/tests/auth_test.rs";
        let (detail, named) = refused(WriteScope::TestsOnly, &[climbing], &rust_globs());
        assert_eq!(
            named,
            [climbing],
            "a path that walks out of the worktree is refused even though its tail matches a              test glob",
        );
        assert_eq!(
            detail, UNLOCATABLE_RULE,
            "the refusal says the path could not be placed, which is the reason the glob was              never asked",
        );
    }

    #[test]
    fn a_scope_that_grants_the_worktree_grants_nothing_outside_it() {
        let (detail, named) = refused(WriteScope::All, &["/etc/hosts"], &rust_globs());
        assert_eq!(
            named,
            ["/etc/hosts"],
            "`All` is every path in the worktree, and an absolute path is not in it",
        );
        assert_eq!(detail, UNLOCATABLE_RULE);
    }

    #[test]
    fn a_diff_with_both_a_stray_path_and_a_scope_break_quotes_both_rules() {
        let (_, named) = refused(
            WriteScope::TestsOnly,
            &["/etc/hosts", "crates/ktask-core/src/protocol.rs"],
            &rust_globs(),
        );
        assert_eq!(named, ["/etc/hosts", "crates/ktask-core/src/protocol.rs"]);
        let (detail, _) = refused(
            WriteScope::TestsOnly,
            &["crates/ktask-core/src/protocol.rs", "../elsewhere/notes.md"],
            &rust_globs(),
        );
        assert_eq!(
            detail,
            format!("{SCOPE_RULE}; {UNLOCATABLE_RULE}"),
            "one refusal has to say both what was written outside the scope and that a path              could not be located, or a reader is sent to fix half of it",
        );
    }

    #[test]
    fn a_path_that_walks_back_inside_the_worktree_is_the_path_it_names() {
        // `..` inside the root is a path, not an escape: dropping it is what
        // keeps this refusal from refusing paths git itself would print.
        permitted(
            WriteScope::TestsOnly,
            &[
                "crates/ktask-core/src/../tests/protocol.rs",
                "./tests/terminal.rs",
            ],
            &rust_globs(),
        );
    }

    #[test]
    fn one_star_stays_inside_one_directory() {
        let narrow = glob_list(&["tests/*.rs"]);
        permitted(WriteScope::TestsOnly, &["tests/protocol.rs"], &narrow);
        let (_, named) = refused(
            WriteScope::TestsOnly,
            &["tests/fixture/protocol.rs"],
            &narrow,
        );
        assert_eq!(
            named,
            ["tests/fixture/protocol.rs"],
            "`*` crossing a directory boundary would turn one typed glob into `**`",
        );
    }

    #[test]
    fn a_double_star_stands_for_any_number_of_directories_including_none() {
        let module = glob_list(&["src/**/tests.rs"]);
        permitted(
            WriteScope::TestsOnly,
            &[
                "src/tests.rs",
                "src/store/tests.rs",
                "src/store/memory/tests.rs",
            ],
            &module,
        );
        let (_, named) = refused(
            WriteScope::TestsOnly,
            &["tests.rs", "src/tests.rs.bak"],
            &module,
        );
        assert_eq!(
            named,
            ["tests.rs", "src/tests.rs.bak"],
            "a pattern is anchored where it was written and a name is matched whole: `src/` is              literal, and `tests.rs.bak` is not `tests.rs`",
        );
    }

    #[test]
    fn a_question_mark_stands_for_exactly_one_character() {
        let helpers = glob_list(&["tests/fixture?.rs"]);
        permitted(WriteScope::TestsOnly, &["tests/fixture1.rs"], &helpers);
        let (_, named) = refused(
            WriteScope::TestsOnly,
            &["tests/fixture12.rs", "tests/fixture.rs"],
            &helpers,
        );
        assert_eq!(
            named,
            ["tests/fixture12.rs", "tests/fixture.rs"],
            "`?` matching a run, or nothing, is a glob wider than the one written",
        );
    }

    #[test]
    fn a_name_that_is_not_text_is_never_a_test_path() {
        // A glob is text, so a name that is not UTF-8 matches no pattern — it
        // stays a path, and an implementation phase keeps its scope over it.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let odd = PathBuf::from(OsStr::from_bytes(b"tests/fixture\xff.rs"));
        let changed = std::slice::from_ref(&odd);
        let error = check_scope(WriteScope::TestsOnly, changed, &rust_globs())
            .expect_err("a name that is not text is not a test path, whoever wrote the glob");
        assert!(matches!(error, Error::Policy { .. }), "{error}");
        if let Err(error) = check_scope(WriteScope::All, changed, &rust_globs()) {
            panic!("an implementation phase lost its scope over a path it can name: {error}");
        }
    }

    #[test]
    fn braces_and_bracketed_classes_match_literally() {
        // Pinned so the absence of a brace expansion is a decision rather than an
        // oversight: a glob language the configuration did not get has to refuse,
        // not silently widen what a red phase may write.
        let braces = glob_list(&["**/*.{rs,test}"]);
        let (_, named) = refused(
            WriteScope::TestsOnly,
            &["src/auth.rs", "src/auth.test"],
            &braces,
        );
        assert_eq!(named, ["src/auth.rs", "src/auth.test"]);
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

    /// What one targeted run reported, spelled by the tests it named as failing.
    ///
    /// The counts are [`crate::parse_cargo`]'s own — one name per failing test —
    /// so a fixture cannot drift into claiming a failure it does not list.
    fn report(failures: &[&str]) -> TestSummary {
        TestSummary {
            passed: 1,
            failed: u32::try_from(failures.len()).expect("a fixture cannot list a negative count"),
            ignored: 0,
            failures: failures.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    /// Assert `verify_red(before, after)` refuses, and answer with the sentence it
    /// refused by. Named apart from `refused` above, which refuses a write scope.
    fn refused_red(before: &TestSummary, after: &TestSummary) -> String {
        let error = verify_red(before, after)
            .expect_err("a run with no newly failing test is refused, not accepted");
        let Error::Gate { kind, detail } = error else {
            panic!("a red phase that found no new failure is refused by its gate, got {error}");
        };
        assert_eq!(
            kind,
            GateKind::Targeted.to_string(),
            "the gate the red phase declares is the one that refuses it, not `{kind}`"
        );
        detail
    }

    #[test]
    fn a_test_that_fails_after_and_not_before_is_the_new_failure() {
        let new = "store::tests::retry_refuses_a_task_that_is_not_active";
        let found = verify_red(&report(&[]), &report(&[new])).expect(
            "a phase that started with nothing failing and ended with a test failing is red",
        );
        assert_eq!(
            found,
            vec![new.to_owned()],
            "the phase started with nothing failing and ended with one test failing: that name \
             is the evidence §9 asks red to leave behind"
        );
    }

    #[test]
    fn an_unchanged_failure_set_is_refused_and_says_what_it_compared() {
        let old = "store::tests::publish_refuses_a_dirty_tree";
        assert_eq!(
            refused_red(&report(&[old]), &report(&[old])),
            format!(
                "a red phase has to leave a test that failed after the change and did not fail \
                 before it; the run found none (failing before: `{old}`; failing after: \
                 `{old}`)"
            ),
            "the same failure before and after is a test that was already broken, and the \
             refusal has to quote the comparison an operator can check"
        );
    }

    #[test]
    fn a_run_that_lists_no_failing_test_at_all_is_refused() {
        let detail = refused_red(&report(&[]), &report(&[]));
        assert!(detail.contains("failing before: nothing"), "{detail}");
        assert!(detail.contains("failing after: nothing"), "{detail}");
    }

    #[test]
    fn a_failure_that_already_failed_is_not_evidence_of_a_new_test() {
        let old = "journal::tests::append_refuses_a_sequence_gap";
        let older = "journal::tests::read_hands_over_one_record_and_keeps_none";
        let fresh = "journal::tests::rebuild_folds_before_it_touches_the_projection";
        let found = verify_red(&report(&[old, older]), &report(&[old, older, fresh]))
            .expect("a test that was not failing before is failing now");
        assert_eq!(
            found,
            vec![fresh.to_owned()],
            "the two names that failed before the phase say nothing about it; only the name that \
             did not fail before is a test written first"
        );
    }

    #[test]
    fn the_new_names_come_back_in_the_order_the_run_listed_them() {
        let first = "git::tests::fetch_brings_back_the_tip_it_was_asked_for";
        let second = "git::tests::a_conflicted_rebase_is_aborted_before_its_evidence_goes";
        let found = verify_red(&report(&[]), &report(&[second, first]))
            .expect("two newly failing tests is a red phase");
        assert_eq!(
            found,
            vec![second.to_owned(), first.to_owned()],
            "evidence keeps the order the run wrote, so reading the same log twice compares equal"
        );
    }

    #[test]
    fn a_failure_that_stopped_failing_is_not_a_new_one() {
        // The count moved, and the count is not the question: the phase repaired
        // one old failure and left the other standing, and neither is a test that
        // was written first.
        let kept = "git::tests::commit_refuses_an_empty_tree";
        let repaired = "git::tests::a_lock_names_its_holder";
        assert!(
            refused_red(&report(&[repaired, kept]), &report(&[kept]))
                .contains("the run found none"),
            "a count that fell is not a test that was newly written to fail"
        );
    }

    #[test]
    fn a_name_two_binaries_both_reported_is_returned_once() {
        let twice = "task::tests::parse_plan_refuses_an_unknown_protocol";
        let found = verify_red(&report(&[]), &report(&[twice, twice]))
            .expect("a failing test is a failing test whoever listed it");
        assert_eq!(
            found,
            vec![twice.to_owned()],
            "two test binaries can each hold a test of the same name, and the green phase re-runs \
             a test by name, so the evidence names it once"
        );
    }

    /// What one targeted run reported, spelled by how many tests it says passed
    /// as well as by the names it lists as failing. Green reads both halves of
    /// [`TestSummary`], so its fixture has to be able to say "ran nothing".
    fn report_running(passed: u32, failures: &[&str]) -> TestSummary {
        TestSummary {
            passed,
            failed: u32::try_from(failures.len()).expect("a fixture cannot list a negative count"),
            ignored: 0,
            failures: failures.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    /// `expected`, as the owned list the signature asks for.
    fn named(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Assert `verify_green(named, after)` refuses, and answer with the sentence
    /// it refused by.
    fn refused_green(expected: &[&str], after: &TestSummary) -> String {
        let error = verify_green(&named(expected), after)
            .expect_err("a green phase whose run cannot confirm it is refused, not accepted");
        let Error::Gate { kind, detail } = error else {
            panic!(
                "a green phase refused by its own targeted check is refused as a gate failure, got {error}"
            );
        };
        assert_eq!(
            kind,
            GateKind::Targeted.to_string(),
            "the gate the green phase declares is the one that refuses it, not `{kind}`"
        );
        detail
    }

    #[test]
    fn the_named_test_passing_and_nothing_else_failing_is_a_green_phase() {
        let new = "protocol::tests::verify_green_refuses_a_regression_by_name";
        verify_green(&named(&[new]), &report_running(11, &[])).expect(
            "the run listed no failing test and reported enough passing tests to cover the one \
             it was asked about: that is §9 step 4 said out loud",
        );
    }

    #[test]
    fn a_phase_named_no_test_and_the_run_named_no_failure_is_accepted() {
        // Emptiness is red's refusal, not green's: `verify_red` cannot hand back
        // an empty list, so a caller that arrives with no names has nothing to
        // confirm and a run that failed nothing to say so with.
        verify_green(&named(&[]), &report_running(0, &[]))
            .expect("no name to confirm and no failure to answer for is a phase that passed");
    }

    #[test]
    fn a_test_the_phase_was_for_that_still_fails_refuses_the_phase_and_names_it() {
        let new = "protocol::tests::verify_green_refuses_a_regression_by_name";
        assert_eq!(
            refused_green(&[new], &report_running(11, &[new])),
            format!(
                "{GREEN_RULE}; the run reported (expected and still failing: `{new}`; passing \
                 before and failing now: nothing)"
            ),
            "the test the phase existed for is still failing, and the refusal has to name it \
             instead of reporting a phase that merely failed to confirm"
        );
    }

    #[test]
    fn a_regression_outside_the_named_tests_refuses_the_phase_and_names_the_test() {
        let new = "protocol::tests::verify_green_refuses_a_regression_by_name";
        let broke = "journal::tests::append_refuses_a_sequence_gap";
        assert_eq!(
            refused_green(&[new], &report_running(11, &[broke])),
            format!(
                "{GREEN_RULE}; the run reported (expected and still failing: nothing; passing \
                 before and failing now: `{broke}`)"
            ),
            "green holds the whole tree open, so a test that was passing when red ended and fails \
             now is this phase's own doing, and it is refused by name"
        );
    }

    #[test]
    fn a_refusal_names_every_test_that_stayed_red_and_every_one_the_phase_broke() {
        let kept = "protocol::tests::verify_green_refuses_a_regression_by_name";
        let also = "protocol::tests::verify_green_accepts_a_run_that_confirms_it";
        let broke = "git::tests::fetch_brings_back_the_tip_it_was_asked_for";
        let detail = refused_green(&[kept, also], &report_running(9, &[broke, kept]));
        assert!(
            detail.contains(&format!("expected and still failing: `{kept}`")),
            "the name the phase was for is refused by name, and the one that passed is not \
             named at all: {detail}"
        );
        assert!(
            !detail.contains(also),
            "{also} passed, so the refusal has no business naming it: {detail}"
        );
        assert!(
            detail.contains(&format!("passing before and failing now: `{broke}`")),
            "the name the phase broke is refused by name: {detail}"
        );
    }

    #[test]
    fn a_name_two_binaries_both_reported_is_named_once_in_the_refusal() {
        let twice = "task::tests::parse_plan_refuses_an_unknown_protocol";
        assert_eq!(
            refused_green(&[], &report_running(5, &[twice, twice])),
            format!(
                "{GREEN_RULE}; the run reported (expected and still failing: nothing; passing \
                 before and failing now: `{twice}`)"
            ),
            "one test broke, and a refusal that names it twice reads as two"
        );
    }

    #[test]
    fn a_run_that_reported_too_few_passing_tests_to_have_passed_them_is_refused() {
        let first = "protocol::tests::verify_green_refuses_a_regression_by_name";
        let second = "protocol::tests::verify_green_accepts_a_run_that_confirms_it";
        assert_eq!(
            refused_green(&[first, second], &report_running(1, &[])),
            format!(
                "{GREEN_RULE}; the run reported 1 passing tests against the 2 named to be \
                 confirmed (`{first}`, `{second}`), so at least one of them did not pass"
            ),
            "a name is absent from the failure list either because it passed or because it never \
             ran, and only the count can tell those two apart here"
        );
    }

    #[test]
    fn a_re_run_that_ran_nothing_at_all_is_refused_rather_than_read_as_a_pass() {
        // The dodge the count floor exists for: rename the test, delete it, or
        // give it an `#[ignore]`, and the same filter now matches nothing. An
        // empty run reads as green to a check that only looks for failures.
        let new = "protocol::tests::verify_green_refuses_a_regression_by_name";
        assert!(
            refused_green(&[new], &report_running(0, &[]))
                .contains("so at least one of them did not pass"),
            "a run that ran nothing passed nothing, so {new} did not pass"
        );
    }

    #[test]
    fn a_name_expected_twice_counts_once_against_the_runs_passing_count() {
        // The floor is over names, not list entries: one test is one thing to
        // prove (as in `verify_red`), so a name written twice is covered by one
        // passing test and refused by the run that had none.
        let twice = "protocol::tests::verify_green_refuses_a_regression_by_name";
        verify_green(&named(&[twice, twice]), &report_running(1, &[]))
            .expect("the same name written twice is one test to pass, and the run passed one test");
        assert!(
            refused_green(&[twice, twice], &report_running(0, &[]))
                .contains("the 1 named to be confirmed"),
            "the refusal counts the name, not the two entries it was written in: \
             the run reported nothing passing"
        );
    }
}
